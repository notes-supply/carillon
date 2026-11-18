//! What storage shows an interface of its conversations: how they order,
//! what each is called, and what a turn says of its message and its ending.

use carillon_protocol::{
    conversations::TurnSource,
    model::{AnswerFile, StagedAttachments, StagedFile, TurnAttachment},
    observation::{
        Conversation, ConversationKey, Ending, Limit, ObservedTurn, OpenView, Opened as Shown,
        Progress, SNAPSHOT_LIMIT, View,
    },
    turns::{Silence, TurnOpening},
};

use super::{super::Opened, *};
use crate::{answers::KeptFile, attachments::Capture, db::TooLarge};

/// Accepts `event` from `interface` and returns its turn.
fn say(database: &Database, interface: &str, event: &ExternalEvent) -> Result<String> {
    let admission = Admission::Queued {
        providers: &json!(["provider snapshot"]),
        resource_snapshot_id: "resource snapshot",
    };
    Ok(database
        .accept_event(interface, "runtime", event, admission, None)?
        .turn_id)
}

/// Accepts a message saying `text` in the conversation with `audience`.
fn write(database: &Database, id: &str, audience: &str, text: &str) -> Result<String> {
    let event = ExternalEvent {
        content: text.to_owned(),
        ..event(id, audience)
    };
    say(database, "discord", &event)
}

/// The runtime opens a turn in the conversation with `audience`.
fn open(database: &Database, audience: &str, key: &str, label: Option<&str>) -> Result<String> {
    let opening = TurnOpening {
        interface_id: "discord".to_owned(),
        agent_id: "agent".to_owned(),
        audience_id: audience.to_owned(),
        key: key.to_owned(),
        label: label.map(str::to_owned),
    };
    let Opened::Receipt(opened) = database.open_turn("runtime", Some("runtime"), &opening)? else {
        bail!("no turn opened");
    };
    Ok(opened.receipt.turn_id)
}

fn settle(database: &Database, turn_id: &str, settlement: Settlement<'_>) -> Result<()> {
    let settled = database.settle_held("runtime", turn_id, settlement)?;
    assert_eq!(settled, Settled::Recorded);
    Ok(())
}

/// The interface's conversations, the latest active first.
fn listed(database: &Database, interface: &str) -> Result<Vec<Conversation>> {
    Ok(database
        .observed(interface, &View::default())?
        .conversations)
}

/// The audiences of the conversations `discord` lists, in its order.
fn order(database: &Database) -> Result<Vec<String>> {
    Ok(listed(database, "discord")?
        .into_iter()
        .map(|conversation| conversation.key.audience_id().to_owned())
        .collect())
}

/// The turns `discord` is shown of its conversation with `audience`.
fn turns(database: &Database, audience: &str) -> Result<Vec<ObservedTurn>> {
    let view = View {
        conversations: Limit::default(),
        open: Some(OpenView {
            conversation: ConversationKey::new("agent".to_owned(), audience.to_owned())?,
            turns: Limit::default(),
        }),
    };
    match database.observed("discord", &view)?.open {
        Some(Shown::Recorded { turns, .. }) => Ok(turns),
        other => bail!("no recorded conversation is open: {other:?}"),
    }
}

#[test]
fn conversations_are_listed_by_whichever_last_gained_or_ended_a_turn() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    let slow = write(&database, "one", "a", "Plan the trip\nwith two stops")?;
    write(&database, "two", "b", "Second")?;
    say(&database, "other", &event("foreign", "elsewhere"))?;
    assert_eq!(order(&database)?, ["b", "a"]);
    answer(&database, &slow, "A long answer, landing late.")?;
    assert_eq!(order(&database)?, ["a", "b"]);

    let report = open(&database, "b", "nightly", Some("The nightly summary."))?;
    assert_eq!(order(&database)?, ["b", "a"]);
    let running = write(&database, "three", "a", "While that runs")?;
    assert_eq!(order(&database)?, ["a", "b"]);
    answer(&database, &report, "All quiet.")?;
    assert_eq!(order(&database)?, ["b", "a"]);

    let steered = write(&database, "four", "a", "Actually")?;
    settle(&database, &steered, Settlement::Silent(Silence::Steered))?;
    write(&database, "five", "b", "Meanwhile")?;
    assert_eq!(order(&database)?, ["b", "a"]);
    answer(&database, &running, "The answer to both.")?;
    assert_eq!(order(&database)?, ["a", "b"]);

    let [a, b] = listed(&database, "discord")?
        .try_into()
        .map_err(|listed| anyhow::anyhow!("expected two conversations: {listed:?}"))?;
    assert_eq!(
        (a.key.agent_id(), a.channel_id.as_str(), a.title.as_str()),
        ("agent", "channel", "Plan the trip")
    );
    assert_eq!((a.at_work, b.at_work), (0, 2));
    assert!(a.started_at > 0 && a.started_at <= a.active_at);
    let foreign = listed(&database, "other")?;
    assert_eq!(foreign.len(), 1);
    assert_eq!(foreign[0].key.audience_id(), "elsewhere");
    assert!(listed(&database, "unknown")?.is_empty());
    Ok(())
}

#[test]
fn a_conversations_activity_rises_with_each_of_its_own_turns_events_alone() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    let activity = |audience: &str| -> Result<i64> {
        Ok(listed(&database, "discord")?
            .iter()
            .find(|conversation| conversation.key.audience_id() == audience)
            .context("not listed")?
            .activity)
    };
    let first = write(&database, "one", "watched", "hello")?;
    let mut seen = vec![activity("watched")?];
    let mut rose = |event: &str| -> Result<()> {
        let now = activity("watched")?;
        assert!(seen.last().is_some_and(|last| *last < now), "{event}");
        seen.push(now);
        Ok(())
    };
    let second = write(&database, "two", "watched", "and again")?;
    rose("a second message")?;
    answer(&database, &first, "done")?;
    rose("an answer")?;
    settle(&database, &second, Settlement::Silent(Silence::Stopped))?;
    rose("a silence")?;
    let opened = open(&database, "watched", "nightly", None)?;
    rose("a turn its runtime opened")?;
    let failure = Settlement::Failed {
        error: "runtime failed",
        notice: "That turn was interrupted.",
        activity: None,
    };
    settle(&database, &opened, failure)?;
    rose("a failure")?;

    let settled = activity("watched")?;
    write(&database, "one", "watched", "a repeated event")?;
    write(&database, "elsewhere", "another", "hello")?;
    hold(&database, "runtime")?;
    say(&database, "other", &event("foreign", "watched"))?;
    assert_eq!(activity("watched")?, settled);
    assert!(activity("another")? > settled);
    Ok(())
}

#[test]
fn a_title_is_the_first_line_that_says_anything_within_its_length() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    let long = "é".repeat(200);
    for (audience, text) in [
        ("plain", "Hello there"),
        ("padded", "\n   \n  Indented start  \nrest"),
        ("long", long.as_str()),
        ("files", ""),
    ] {
        write(&database, audience, audience, text)?;
    }
    write(&database, "later", "plain", "Something else")?;
    let mut titles = listed(&database, "discord")?
        .into_iter()
        .map(|conversation| {
            (
                conversation.key.audience_id().to_owned(),
                conversation.title,
            )
        })
        .collect::<Vec<_>>();
    titles.sort();
    assert_eq!(
        titles,
        [
            ("files".to_owned(), String::new()),
            ("long".to_owned(), "é".repeat(80)),
            ("padded".to_owned(), "Indented start".to_owned()),
            ("plain".to_owned(), "Hello there".to_owned()),
        ]
    );
    Ok(())
}

/// Nothing of `kept_back` is in what an interface is shown of `turns`.
fn keeps_back(turns: &[ObservedTurn], kept_back: &[&str]) -> Result<()> {
    let wire = serde_json::to_string(turns)?;
    for text in kept_back
        .iter()
        .chain(&["provider snapshot", "resource snapshot"])
    {
        assert!(!wire.contains(text), "{text} was shown");
    }
    Ok(())
}

#[test]
fn a_message_is_shown_as_written_with_its_files_and_its_answer_s_in_order() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    let staged = |name: &str, size, content_type: Option<&str>| StagedFile {
        name: name.to_owned(),
        size,
        content_type: content_type.map(str::to_owned),
    };
    let message = ExternalEvent {
        content: "Look at these".to_owned(),
        native: json!({"context": "quoted for the model"}),
        attachments: Some(StagedAttachments {
            batch: "batch".to_owned(),
            files: vec![
                staged("one.txt", 1, Some("text/plain")),
                staged("two.png", 2, None),
            ],
        }),
        ..event("files", "conversation")
    };
    let capture = Capture {
        id: "sent".to_owned(),
        digests: vec!["first digest".to_owned(), "second digest".to_owned()],
    };
    let admission = Admission::Queued {
        providers: &json!(["provider snapshot"]),
        resource_snapshot_id: "resource snapshot",
    };
    let turn = database
        .accept_event("discord", "runtime", &message, admission, Some(&capture))?
        .turn_id;
    let kept = |name: &str, size| KeptFile {
        capture: "kept".to_owned(),
        name: name.to_owned(),
        source: format!("/work/{name}"),
        size,
        sha256: format!("{name} digest"),
    };
    let final_report = report(3)?;
    let content = "Here.\n\nCould not attach big.bin: it is too large.";
    settle(
        &database,
        &turn,
        Settlement::Answered {
            output: "Here.",
            content,
            activity: Some(&final_report),
            files: &[kept("b.pdf", 20), kept("a.pdf", 10)],
        },
    )?;

    let sent =
        |ordinal: u32, name: &str, size, content_type: Option<&str>, sha256: &str| TurnAttachment {
            path: format!("sent/{ordinal}"),
            name: name.to_owned(),
            size,
            content_type: content_type.map(str::to_owned),
            sha256: sha256.to_owned(),
        };
    let attached = |ordinal: u32, name: &str, size| AnswerFile {
        path: format!("kept/{ordinal}"),
        name: name.to_owned(),
        source: format!("/work/{name}"),
        size,
        sha256: format!("{name} digest"),
    };
    let source = TurnSource::Message {
        event_id: "files".to_owned(),
        text: "Look at these".to_owned(),
        command: false,
        files: vec![
            sent(0, "one.txt", 1, Some("text/plain"), "first digest"),
            sent(1, "two.png", 2, None, "second digest"),
        ],
    };
    let shown = turns(&database, "conversation")?;
    let [only] = shown.as_slice() else {
        bail!("expected one turn: {shown:?}");
    };
    assert_eq!(
        (only.turn_id.as_str(), &only.source),
        (turn.as_str(), &source)
    );
    let Progress::Ended { ending, report, .. } = &only.progress else {
        bail!("not ended: {only:?}");
    };
    assert_eq!(
        (ending, report),
        (
            &Ending::Answered {
                text: content.to_owned(),
                files: vec![attached(0, "b.pdf", 20), attached(1, "a.pdf", 10)],
            },
            &Some(final_report)
        )
    );
    keeps_back(&shown, &["quoted for the model"])
}

#[test]
fn commands_silences_and_a_runtime_s_own_turns_are_shown_apart() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    let command = ExternalEvent {
        content: "/model".to_owned(),
        native: json!({"intent": "command"}),
        ..event("command", "conversation")
    };
    let failed = say(&database, "discord", &command)?;
    let failure = Settlement::Failed {
        error: "operator detail",
        notice: "That turn was interrupted.",
        activity: None,
    };
    settle(&database, &failed, failure)?;
    let steered = write(&database, "steered", "conversation", "Actually")?;
    settle(&database, &steered, Settlement::Silent(Silence::Steered))?;
    let stopped = write(&database, "stopped", "conversation", "Never mind")?;
    settle(&database, &stopped, Settlement::Silent(Silence::Stopped))?;
    let label = "The nightly summary.";
    let labelled = open(&database, "conversation", "nightly", Some(label))?;
    let unlabelled = open(&database, "conversation", "report", None)?;
    let queued = write(&database, "queued", "conversation", "Still there?")?;

    let message = |event_id: &str, text: &str, command| TurnSource::Message {
        event_id: event_id.to_owned(),
        text: text.to_owned(),
        command,
        files: Vec::new(),
    };
    let opened = |label: Option<&str>| TurnSource::Opened {
        label: label.map(str::to_owned),
    };
    let ended = |ending| Progress::Ended {
        finished_at: 0,
        ending,
        report: None,
    };
    let interrupted = Ending::Interrupted {
        notice: "That turn was interrupted.".to_owned(),
    };
    let running = || Progress::Running { activity: None };
    let expected = [
        (
            failed,
            message("command", "/model", true),
            ended(interrupted),
        ),
        (
            steered,
            message("steered", "Actually", false),
            ended(Ending::Steered),
        ),
        (
            stopped,
            message("stopped", "Never mind", false),
            ended(Ending::Stopped),
        ),
        (labelled, opened(Some(label)), running()),
        (unlabelled, opened(None), running()),
        (
            queued,
            message("queued", "Still there?", false),
            Progress::Queued,
        ),
    ];
    let shown = turns(&database, "conversation")?;
    let came = shown.iter().map(|turn| {
        let mut progress = turn.progress.clone();
        if let Progress::Ended { finished_at, .. } = &mut progress {
            *finished_at = 0;
        }
        (turn.turn_id.clone(), turn.source.clone(), progress)
    });
    assert_eq!(came.collect::<Vec<_>>(), expected);
    keeps_back(&shown, &["operator detail"])
}

#[test]
fn the_read_of_a_window_stops_at_the_turn_its_files_pass_the_snapshot_limit() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    // One byte of text and 100 files each, with the longest names and
    // media types core admits: together over the limit, the text far under.
    let files = (0..100)
        .map(|ordinal| StagedFile {
            name: format!("{ordinal:03}{}", "n".repeat(1021)),
            size: 0,
            content_type: Some("t".repeat(255)),
        })
        .collect::<Vec<_>>();
    let capture = Capture {
        id: "capture".to_owned(),
        digests: vec!["digest".to_owned(); files.len()],
    };
    for index in 0..130 {
        let message = ExternalEvent {
            content: "x".to_owned(),
            attachments: Some(StagedAttachments {
                batch: "batch".to_owned(),
                files: files.clone(),
            }),
            ..event(&index.to_string(), "conversation")
        };
        let admission = Admission::Queued {
            providers: &json!([]),
            resource_snapshot_id: "resources",
        };
        database.accept_event("discord", "runtime", &message, admission, Some(&capture))?;
    }
    let window = |turns| -> Result<View> {
        Ok(View {
            conversations: Limit::default(),
            open: Some(OpenView {
                conversation: ConversationKey::new("agent".to_owned(), "conversation".to_owned())?,
                turns: Limit::new(turns)?,
            }),
        })
    };
    let Some(Shown::Recorded { turns: newest, .. }) =
        database.observed("discord", &window(1)?)?.open
    else {
        bail!("the conversation is not recorded");
    };
    let turn = serde_json::to_vec(&newest[0])?.len();
    let refused = database
        .observed("discord", &window(130)?)
        .err()
        .context("a window over the limit was read whole")?;
    let TooLarge(size) = refused.downcast()?;
    assert!(size > SNAPSHOT_LIMIT, "{size}");
    assert!(
        size <= SNAPSHOT_LIMIT + turn,
        "{size} is more than a turn over"
    );
    Ok(())
}
