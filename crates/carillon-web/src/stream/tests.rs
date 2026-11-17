//! The page's stream, read as a page reads it, over a fake of core that
//! streams as core does.

use std::{fs::OpenOptions, path::PathBuf, time::Duration};

use anyhow::{Context as _, Result};
use axum::http::StatusCode;
use carillon_protocol::{
    conversations::TurnSource,
    model::{ActivityPhase, AnswerFile, TurnActivity, TurnAttachment},
    observation::{Ending, KEEP_ALIVE, ObservedTurn, Progress},
    turns::ControlAction,
};
use serde_json::{Value, json};
use tokio::{sync::oneshot, task::JoinHandle, time::Instant};

use crate::{
    browser::{now, set_password},
    fixture::{Fixture, PASSWORD, Page, conversation, key},
};

const CHAT: &str = "0190f3a8-7b1c-7def-8a2b-3c4d5e6f7a8b";
const OTHER: &str = "0190f3a8-7b1c-7def-8a2b-000000000001";
const DAY: u64 = 24 * 60 * 60;
/// The page's sample message, a snapshot with one turn of each kind.
const SAMPLE: &str = include_str!("../../../../packages/web/src/api/stream.sample.json");

fn open(agent: &str) -> String {
    format!("conversations=50&agent_id={agent}&audience_id={CHAT}&turns=50")
}

/// The event ID of a page's message, `m-` and a UUID, as identity.rs admits
/// it.
fn event(ordinal: u8) -> String {
    format!("m-0190f3a8-7b1c-7def-8a2b-{ordinal:012}")
}

fn message(ordinal: u8, text: &str) -> TurnSource {
    TurnSource::Message {
        event_id: event(ordinal),
        text: text.to_owned(),
        command: false,
        files: Vec::new(),
    }
}

fn turn(id: &str, source: TurnSource, progress: Progress) -> ObservedTurn {
    ObservedTurn {
        turn_id: id.to_owned(),
        source,
        accepted_at: 10,
        progress,
    }
}

fn ended(ending: Ending) -> Progress {
    Progress::Ended {
        finished_at: 20,
        ending,
        report: None,
    }
}

fn activity(phase: ActivityPhase, controls: &[ControlAction]) -> TurnActivity {
    TurnActivity {
        phase,
        headline: Some("reading".to_owned()),
        running: Vec::new(),
        recent: Vec::new(),
        tool_calls: 2,
        work: None,
        notes: Vec::new(),
        agent: None,
        partial: false,
        controls: controls.to_vec(),
    }
}

/// What every snapshot of the fixture's installation says of the interface.
fn interface() -> Value {
    json!({
        "agents": [
            {"id": "assistant", "name": "Assistant"},
            {"id": "second", "name": "Second"},
        ],
        "limits": {"files": 3, "file_bytes": 1024 * 1024, "message_bytes": 2 * 1024 * 1024},
        "custom_site": false,
    })
}

/// `snapshot` without the interface's own facts, which are asserted once.
fn shown(mut snapshot: Value) -> Result<Value> {
    let fields = snapshot.as_object_mut().context("no snapshot")?;
    for (name, said) in interface().as_object().context("no interface")? {
        assert_eq!(fields.remove(name).as_ref(), Some(said), "{name}");
    }
    Ok(snapshot)
}

#[tokio::test]
async fn a_stream_begins_with_a_snapshot_of_its_view_and_asks_core_only_when_it_is_read()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let mut page = fixture.stream("conversations=50", &token).await?;
    assert!(
        fixture.core().asked.is_empty(),
        "core was asked before the page read"
    );
    let mut expected = interface();
    expected["conversations"] = json!([]);
    expected["more"] = json!(false);
    expected["open"] = Value::Null;
    assert_eq!(page.snapshot().await?, expected);
    assert_eq!(fixture.core().asked, ["/v1/observe?conversations=50"]);

    for agent in ["second", "nobody"] {
        let mut page = fixture.stream(&open(agent), &token).await?;
        assert_eq!(
            shown(page.snapshot().await?)?,
            json!({
                "conversations": [],
                "more": false,
                "open": {"kind": "unrecorded", "key": {"agent_id": agent, "audience_id": CHAT}},
            })
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_snapshot_follows_each_change_to_what_the_view_shows() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let mut page = fixture.stream(&open("second"), &token).await?;
    page.snapshot().await?;

    {
        let mut core = fixture.core();
        let mut working = conversation("second", CHAT, 7)?;
        working.at_work = 1;
        core.conversations.push(working);
        core.conversations
            .push(conversation("assistant", "not-a-uuid", 5)?);
        core.conversations
            .push(conversation("assistant", OTHER, 3)?);
        core.turns.insert(
            key("second", CHAT)?,
            vec![turn("t1", message(1, "hello"), Progress::Queued)],
        );
    }
    let listed = json!({
        "key": {"agent_id": "second", "audience_id": CHAT},
        "title": "title 7", "started_at": 100, "active_at": 107, "activity": 7, "at_work": 1,
    });
    let queued = json!({
        "turn_id": "t1",
        "source": {
            "kind": "message", "event_id": event(1), "text": "hello", "command": false, "files": [],
        },
        "accepted_at": 10,
        "progress": {"state": "queued"},
    });
    assert_eq!(
        shown(page.snapshot().await?)?,
        json!({
            "conversations": [
                listed,
                {
                    "key": {"agent_id": "assistant", "audience_id": OTHER},
                    "title": "title 3", "started_at": 100, "active_at": 103, "activity": 3,
                    "at_work": 0,
                },
            ],
            "more": false,
            "open": {"kind": "recorded", "conversation": listed, "turns": [queued], "earlier": false},
        })
    );

    fixture
        .core()
        .turns
        .entry(key("second", CHAT)?)
        .or_default()
        .push(turn("t2", message(2, "again"), Progress::Queued));
    let turns = page.snapshot().await?["open"]["turns"].clone();
    assert_eq!(turns.as_array().map(Vec::len), Some(2));
    Ok(())
}

#[tokio::test]
async fn a_change_the_page_is_not_shown_sends_nothing_in_five_seconds_of_a_paused_clock()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    fixture
        .core()
        .conversations
        .push(conversation("second", CHAT, 1)?);
    let mut page = fixture.stream("conversations=50", &token).await?;
    page.snapshot().await?;

    {
        let mut core = fixture.core();
        "another-channel".clone_into(&mut core.conversations[0].channel_id);
        core.conversations
            .push(conversation("assistant", "not-a-uuid", 5)?);
    }
    tokio::time::pause();
    let silent = tokio::time::timeout(Duration::from_secs(5), page.next()).await;
    let silent = silent
        .err()
        .context("the page was sent what it already shows")?;
    assert_eq!(silent.to_string(), "deadline has elapsed");
    tokio::time::resume();

    fixture.core().conversations[0].at_work = 1;
    let listed = page.snapshot().await?["conversations"].clone();
    assert_eq!(listed.as_array().map(Vec::len), Some(1));
    assert_eq!(listed[0]["at_work"], 1);
    Ok(())
}

/// One turn of each kind a person can see.
fn turns(report: &TurnActivity) -> Vec<ObservedTurn> {
    let sent = TurnSource::Message {
        event_id: event(1),
        text: "<b>look</b> at *this*".to_owned(),
        command: true,
        files: vec![TurnAttachment {
            path: "capture-a/1".to_owned(),
            name: "report.pdf".to_owned(),
            size: 8,
            content_type: Some("application/pdf".to_owned()),
            sha256: "ab".repeat(32),
        }],
    };
    let answered = Progress::Ended {
        finished_at: 20,
        ending: Ending::Answered {
            text: "Here is the **chart**. <script>x</script>".to_owned(),
            files: vec![AnswerFile {
                path: "capture-a/0".to_owned(),
                name: "chart.png".to_owned(),
                source: "/home/owner/work/secret-project/chart.png".to_owned(),
                size: 25,
                sha256: "cd".repeat(32),
            }],
        },
        report: Some(report.clone()),
    };
    let scheduled = TurnSource::Opened {
        label: Some("Scheduled run: morning summary".to_owned()),
    };
    let held = activity(
        ActivityPhase::Queued,
        &[ControlAction::Steer, ControlAction::Stop],
    );
    vec![
        turn("t1", sent, answered),
        turn(
            "t2",
            message(2, "and then?"),
            ended(Ending::Interrupted {
                notice: "The runtime ran out of *credit*.".to_owned(),
            }),
        ),
        turn("t3", message(3, "also this"), ended(Ending::Steered)),
        turn("t4", message(4, "never mind"), ended(Ending::Stopped)),
        turn(
            "t5",
            scheduled,
            ended(Ending::Answered {
                text: "Good morning.".to_owned(),
                files: Vec::new(),
            }),
        ),
        turn(
            "t6",
            TurnSource::Opened { label: None },
            Progress::Running {
                activity: Some(activity(ActivityPhase::Model, &[])),
            },
        ),
        turn(
            "t7",
            message(7, "waiting"),
            Progress::Running {
                activity: Some(held),
            },
        ),
        turn(
            "t8",
            message(8, "unanswered"),
            Progress::Running { activity: None },
        ),
        turn("t9", message(9, "last"), Progress::Queued),
    ]
}

/// Nothing in `wire` says where a file is on this host, or how core keeps it.
fn keeps_back(wire: &str, installation: &std::path::Path) -> Result<()> {
    for kept_back in [
        "secret-project",
        "/home/",
        "sha256",
        "\"path\"",
        "content_type",
        "channel",
        installation.to_str().context("path")?,
    ] {
        assert!(!wire.contains(kept_back), "{kept_back} reached the page");
    }
    Ok(())
}

/// `message` with each file link's expiry and signature, which change with
/// the day and the installation, taken out.
fn unsigned(message: &mut Value) -> Result<()> {
    match message {
        Value::Object(fields) => {
            for (name, field) in fields {
                match (name.as_str(), field) {
                    ("url", Value::String(link)) => {
                        let (unsigned, signed) =
                            link.split_once("&exp=").context("a link with no expiry")?;
                        anyhow::ensure!(signed.contains("&sig="), "{link} is not signed");
                        *link = unsigned.to_owned();
                    }
                    (_, field) => unsigned(field)?,
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                unsigned(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[tokio::test]
async fn a_snapshot_of_each_kind_of_turn_is_the_page_s_sample_message() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let store = fixture.directory.path().join("state/attachments");
    std::fs::create_dir_all(store.join("capture-a"))?;
    std::fs::write(
        store.join("capture-a/0"),
        b"\x89PNG\r\n\x1a\nrest of a picture",
    )?;
    std::fs::write(store.join("capture-a/1"), b"%PDF-1.7")?;
    let report = activity(ActivityPhase::Model, &[ControlAction::Stop]);
    {
        let mut core = fixture.core();
        core.conversations.push(conversation("second", CHAT, 9)?);
        core.turns.insert(key("second", CHAT)?, turns(&report));
    }
    let mut page = fixture.stream(&open("second"), &token).await?;
    let mut message = page.next().await?.context("the stream ended")?;
    keeps_back(&message.to_string(), fixture.directory.path())?;
    let mut sample: Value = serde_json::from_str(SAMPLE)?;
    unsigned(&mut message)?;
    unsigned(&mut sample)?;
    assert_eq!(message, sample);
    Ok(())
}

#[tokio::test]
async fn a_window_shows_the_newest_turns_and_says_older_ones_exist() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    {
        let mut core = fixture.core();
        for (activity, audience) in [(1, CHAT), (2, OTHER)] {
            core.conversations
                .push(conversation("second", audience, activity)?);
        }
        let report = activity(ActivityPhase::Model, &[]);
        core.turns.insert(key("second", CHAT)?, turns(&report));
    }
    let view = format!("conversations=1&agent_id=second&audience_id={CHAT}&turns=2");
    let mut page = fixture.stream(&view, &token).await?;
    let snapshot = page.snapshot().await?;
    assert_eq!(snapshot["more"], true);
    assert_eq!(snapshot["conversations"][0]["key"]["audience_id"], OTHER);
    let opened = &snapshot["open"];
    assert_eq!(opened["conversation"]["key"]["audience_id"], CHAT);
    assert_eq!(opened["earlier"], true);
    let ids: Vec<&Value> = opened["turns"]
        .as_array()
        .context("no turns")?
        .iter()
        .map(|turn| &turn["turn_id"])
        .collect();
    assert_eq!(ids, ["t8", "t9"]);
    Ok(())
}

#[tokio::test]
async fn a_new_password_ends_the_stream_as_signed_out_before_its_next_snapshot() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let mut page = fixture.stream("conversations=50", &token).await?;
    page.snapshot().await?;
    set_password(&fixture.password_file(), PASSWORD)?;
    fixture
        .core()
        .conversations
        .push(conversation("second", CHAT, 1)?);
    assert_eq!(page.next().await?, Some(json!({"kind": "signed_out"})));
    assert_eq!(page.next().await?, None, "the stream went on");
    Ok(())
}

#[tokio::test]
async fn a_new_password_ends_a_quiet_stream_within_a_keep_alive_of_a_paused_clock() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let mut page = fixture.stream("conversations=50", &token).await?;
    page.snapshot().await?;
    set_password(&fixture.password_file(), PASSWORD)?;
    tokio::time::pause();
    let changed = Instant::now();
    assert_eq!(page.next().await?, Some(json!({"kind": "signed_out"})));
    assert!(changed.elapsed() <= KEEP_ALIVE, "{:?}", changed.elapsed());
    assert_eq!(page.next().await?, None, "the stream went on");
    Ok(())
}

/// Returns once something opens the named pipe `pipe` to read it. Opening
/// it to write waits for that reader, so it is opened on a thread outside
/// the runtime, which a test that fails before the reader comes does not
/// wait for as it ends.
async fn read_from(pipe: PathBuf) -> Result<()> {
    let (opened, writer) = oneshot::channel();
    std::thread::spawn(move || opened.send(OpenOptions::new().write(true).open(pipe).map(drop)));
    Ok(writer.await??)
}

/// A page's first message, while the provider makes it. The open
/// conversation's turn sent two files that are named pipes. Making a
/// snapshot opens each file it links to, and opening a pipe waits until it
/// is opened to write, so the provider has opened the first and waits at
/// the second until `finish`.
struct Making {
    message: JoinHandle<(Result<Option<Value>>, Page)>,
    held: PathBuf,
}

impl Making {
    async fn start(fixture: &Fixture, token: &str) -> Result<Self> {
        let store = fixture.directory.path().join("state/attachments/piped");
        std::fs::create_dir_all(&store)?;
        let [reached, held] = ["0", "1"].map(|name| store.join(name));
        let piped = std::process::Command::new("mkfifo")
            .args([&reached, &held])
            .status()?;
        assert!(piped.success());
        let files = ["piped/0", "piped/1"].map(|path| TurnAttachment {
            path: path.to_owned(),
            name: "notes.txt".to_owned(),
            size: 5,
            content_type: None,
            sha256: "ab".repeat(32),
        });
        let sent = TurnSource::Message {
            event_id: event(1),
            text: "notes".to_owned(),
            command: false,
            files: files.into(),
        };
        {
            let mut core = fixture.core();
            core.conversations.push(conversation("second", CHAT, 1)?);
            core.turns.insert(
                key("second", CHAT)?,
                vec![turn("t1", sent, Progress::Queued)],
            );
        }
        let mut page = fixture.stream(&open("second"), token).await?;
        let mut message = tokio::spawn(async move { (page.next().await, page) });
        tokio::select! {
            opened = read_from(reached) => opened?,
            sent = &mut message => {
                anyhow::bail!("a snapshot was made without opening its files: {:?}", sent?.0)
            }
        }
        Ok(Self { message, held })
    }

    /// The message the page is sent once the provider may finish.
    async fn finish(self) -> Result<(Option<Value>, Page)> {
        read_from(self.held).await?;
        let (message, page) = self.message.await?;
        Ok((message?, page))
    }
}

#[tokio::test]
async fn a_new_password_while_a_snapshot_is_made_ends_the_stream_as_signed_out_instead()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let making = Making::start(&fixture, &token).await?;
    set_password(&fixture.password_file(), PASSWORD)?;
    let (message, mut page) = making.finish().await?;
    assert_eq!(message, Some(json!({"kind": "signed_out"})));
    assert_eq!(page.next().await?, None, "the stream went on");
    Ok(())
}

#[tokio::test]
async fn a_core_that_cannot_be_reached_or_refuses_is_unavailable_and_is_asked_once() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    fixture.core().away_for_requests = 1;
    let mut page = fixture.stream("conversations=50", &token).await?;
    assert_eq!(page.next().await?, Some(json!({"kind": "unavailable"})));
    assert_eq!(page.next().await?, None, "the stream went on");
    assert_eq!(
        fixture.core().asked,
        ["/v1/observe?conversations=50"],
        "the provider asked core again"
    );

    fixture.serving.abort();
    while !fixture.serving.is_finished() {
        tokio::task::yield_now().await;
    }
    let mut page = fixture.stream("conversations=50", &token).await?;
    assert_eq!(page.next().await?, Some(json!({"kind": "unavailable"})));
    assert_eq!(page.next().await?, None, "the stream went on");
    Ok(())
}

#[tokio::test]
async fn a_core_that_ends_its_stream_is_unavailable_and_is_not_asked_again() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let mut page = fixture.stream("conversations=50", &token).await?;
    page.snapshot().await?;
    fixture.core().stopping = true;
    assert_eq!(page.next().await?, Some(json!({"kind": "unavailable"})));
    assert_eq!(page.next().await?, None, "the stream went on");
    assert_eq!(
        fixture.core().asked.len(),
        1,
        "the provider asked core again"
    );
    Ok(())
}

#[tokio::test]
async fn a_stream_ends_without_a_last_message_when_the_provider_stops() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let mut page = fixture.stream("conversations=50", &token).await?;
    page.snapshot().await?;
    fixture.web.stopping.cancel();
    assert_eq!(page.next().await?, None);
    Ok(())
}

#[tokio::test]
async fn a_stream_ends_without_a_last_message_when_its_utc_day_ends_on_a_paused_clock() -> Result<()>
{
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let store = fixture.directory.path().join("state/attachments/capture-a");
    std::fs::create_dir_all(&store)?;
    std::fs::write(store.join("0"), b"notes")?;
    {
        let mut core = fixture.core();
        core.conversations.push(conversation("second", CHAT, 1)?);
        let answer = Ending::Answered {
            text: "Here.".to_owned(),
            files: vec![AnswerFile {
                path: "capture-a/0".to_owned(),
                name: "notes.txt".to_owned(),
                source: "/home/owner/notes.txt".to_owned(),
                size: 5,
                sha256: "cd".repeat(32),
            }],
        };
        core.turns.insert(
            key("second", CHAT)?,
            vec![turn("t1", message(1, "notes?"), ended(answer))],
        );
    }
    let before = now();
    let mut page = fixture.stream(&open("second"), &token).await?;
    let snapshot = page.snapshot().await?;
    let link = &snapshot["open"]["turns"][0]["progress"]["ending"]["files"][0]["url"];
    let expires: u64 = link
        .as_str()
        .and_then(|link| link.split('&').find_map(|pair| pair.strip_prefix("exp=")))
        .context("no expiry")?
        .parse()?;
    assert_eq!(expires % DAY, 0);
    let day_ends = expires - DAY;
    assert!(before < day_ends && day_ends <= before + DAY, "{day_ends}");

    tokio::time::pause();
    let read = Instant::now();
    assert_eq!(page.next().await?, None, "the stream said why it ended");
    let waited = read.elapsed();
    assert!(
        waited <= Duration::from_secs(day_ends - before),
        "{waited:?}"
    );
    let least = Duration::from_secs((day_ends - before).saturating_sub(60));
    assert!(waited >= least, "{waited:?}");
    Ok(())
}

#[tokio::test]
async fn a_snapshot_made_past_its_utc_day_is_not_sent_and_the_stream_ends_on_a_paused_clock()
-> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    let making = Making::start(&fixture, &token).await?;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(DAY)).await;
    let (message, _) = making.finish().await?;
    assert_eq!(message, None, "a snapshot was sent after its day");
    Ok(())
}

#[tokio::test]
async fn a_view_the_page_cannot_ask_for_is_refused_in_words_before_core_is_asked() -> Result<()> {
    let fixture = Fixture::new().await?;
    let token = fixture.sign_in().await?;
    for (view, said) in [
        ("conversations=0", "a window holds 1 to 500 items"),
        ("conversations=501", "a window holds 1 to 500 items"),
        ("conversations=many", "invalid digit"),
        ("conversations=50&turns=50", "needs the conversation"),
        (
            "conversations=50&agent_id=second",
            "its agent and its audience",
        ),
        (
            &format!("conversations=50&audience_id={CHAT}"),
            "its agent and its audience",
        ),
        (
            &format!("conversations=50&agent_id=&audience_id={CHAT}"),
            "its agent and its audience",
        ),
        (
            &format!("conversations=50&agent_id=second&audience_id={CHAT}&turns=501"),
            "a window holds 1 to 500 items",
        ),
        (
            "conversations=50&agent_id=second&audience_id=0190F3A8-7B1C-7DEF-8A2B-3C4D5E6F7A8B",
            "a conversation is a lowercase UUID",
        ),
        (
            "conversations=50&agent_id=second&audience_id=web-chat",
            "a conversation is a lowercase UUID",
        ),
        ("conversations=50&before=7", "unknown field"),
    ] {
        let answer = fixture.get(&format!("/api/stream?{view}"), &token).await?;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{view}");
        assert!(
            answer.header("content-type").starts_with("text/plain"),
            "{view}"
        );
        assert!(answer.text().contains(said), "{view}: {}", answer.text());
    }
    assert!(fixture.core().asked.is_empty(), "core was asked");
    Ok(())
}
