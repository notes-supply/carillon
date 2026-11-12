use carillon_protocol::{
    conversations::{
        ConversationPage, ConversationSummary, HISTORY_PAGE_LIMIT, Transcript, TranscriptAnswer,
        TranscriptTurn, TurnSource,
    },
    model::{AnswerFile, StagedAttachments, StagedFile, TurnAttachment},
    turns::{Silence, TurnOpening},
};

use super::{super::Opened, *};
use crate::{answers::KeptFile, attachments::Capture};

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

fn listed(database: &Database, interface: &str) -> Result<Vec<ConversationSummary>> {
    Ok(database.conversations(interface, None)?.conversations)
}

/// The audiences of the conversations `discord` lists, in its order.
fn order(database: &Database) -> Result<Vec<String>> {
    Ok(listed(database, "discord")?
        .into_iter()
        .map(|conversation| conversation.audience_id)
        .collect())
}

fn transcript(database: &Database, audience: &str) -> Result<Vec<TranscriptTurn>> {
    Ok(database
        .transcript("discord", "agent", audience, None)?
        .turns)
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
        (a.agent_id.as_str(), a.channel_id.as_str(), a.title.as_str()),
        ("agent", "channel", "Plan the trip")
    );
    assert_eq!((a.at_work, b.at_work), (0, 2));
    assert!(a.started_at > 0 && a.started_at <= a.active_at);
    let foreign = listed(&database, "other")?;
    assert_eq!(foreign.len(), 1);
    assert_eq!(foreign[0].audience_id, "elsewhere");
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
            .find(|conversation| conversation.audience_id == audience)
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
        .map(|conversation| (conversation.audience_id, conversation.title))
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

#[test]
fn the_conversation_list_pages_below_the_last_one_listed_while_others_move_up() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    let mut newest_first = Vec::new();
    for index in 0..HISTORY_PAGE_LIMIT + 3 {
        let audience = format!("conversation-{index}");
        write(&database, &audience, &audience, "hello")?;
        newest_first.insert(0, audience);
    }
    let audiences = |page: &ConversationPage| {
        page.conversations
            .iter()
            .map(|conversation| conversation.audience_id.clone())
            .collect::<Vec<_>>()
    };
    let page = database.conversations("discord", None)?;
    assert_eq!(audiences(&page), newest_first[..HISTORY_PAGE_LIMIT]);
    assert_eq!(
        page.next,
        page.conversations
            .last()
            .map(|conversation| conversation.activity)
    );
    let (listed, unlisted) = (&newest_first[3], &newest_first[HISTORY_PAGE_LIMIT + 1]);
    write(&database, "again", listed, "more")?;
    write(&database, "later", unlisted, "more")?;
    let rest = database.conversations("discord", page.next)?;
    assert_eq!(
        audiences(&rest),
        [
            newest_first[HISTORY_PAGE_LIMIT].clone(),
            newest_first[HISTORY_PAGE_LIMIT + 2].clone()
        ]
    );
    assert_eq!(rest.next, None);
    assert_eq!(order(&database)?[..2], [unlisted.clone(), listed.clone()]);
    Ok(())
}

/// What each turn of a transcript says, apart from its times.
type Read<'a> = (
    &'a str,
    &'a TurnSource,
    TurnState,
    &'a Option<TurnActivity>,
    &'a Option<TranscriptAnswer>,
);

fn read(turns: &[TranscriptTurn]) -> Vec<Read<'_>> {
    turns
        .iter()
        .map(|turn| {
            assert_eq!(turn.finished_at.is_some(), turn.answer.is_some());
            (
                turn.turn_id.as_str(),
                &turn.source,
                turn.state,
                &turn.report,
                &turn.answer,
            )
        })
        .collect()
}

/// Nothing of `kept_back` is in what an interface reads of `turns`.
fn keeps_back(turns: &[TranscriptTurn], kept_back: &[&str]) -> Result<()> {
    let wire = serde_json::to_string(turns)?;
    for text in kept_back
        .iter()
        .chain(&["provider snapshot", "resource snapshot"])
    {
        assert!(!wire.contains(text), "{text} was read back");
    }
    Ok(())
}

#[test]
fn a_message_reads_back_as_written_with_its_files_and_its_answers_in_order() -> Result<()> {
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
    let answer = Some(TranscriptAnswer {
        content: content.to_owned(),
        silence: None,
        files: vec![attached(0, "b.pdf", 20), attached(1, "a.pdf", 10)],
        delivery: DeliveryState::Pending,
    });
    let turns = transcript(&database, "conversation")?;
    assert_eq!(
        read(&turns),
        [(
            turn.as_str(),
            &source,
            TurnState::Completed,
            &Some(final_report),
            &answer
        )]
    );
    keeps_back(&turns, &["quoted for the model"])
}

#[test]
fn a_transcript_reads_each_answers_delivery_as_it_stands() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    let turn = write(&database, "one", "conversation", "hello")?;
    answer(&database, &turn, "done")?;
    let delivery = || -> Result<Option<DeliveryState>> {
        let turns = transcript(&database, "conversation")?;
        Ok(turns[0].answer.as_ref().map(|answer| answer.delivery))
    };
    assert_eq!(delivery()?, Some(DeliveryState::Pending));
    let claimed = database
        .claim_delivery("discord", "instance")?
        .context("nothing to claim")?;
    assert_eq!(delivery()?, Some(DeliveryState::Delivering));
    database.acknowledge_delivery(
        "discord",
        claimed.id,
        "instance",
        DeliveryOutcome::Uncertain,
        None,
    )?;
    assert_eq!(delivery()?, Some(DeliveryState::Uncertain));
    Ok(())
}

#[test]
fn a_transcript_tells_commands_silences_and_a_runtimes_own_turns_apart() -> Result<()> {
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
    let told = |content: &str, silence| {
        Some(TranscriptAnswer {
            content: content.to_owned(),
            silence,
            files: Vec::new(),
            delivery: DeliveryState::Pending,
        })
    };
    let (interrupted, completed) = (TurnState::Interrupted, TurnState::Completed);
    let expected = [
        (
            failed,
            message("command", "/model", true),
            interrupted,
            told("That turn was interrupted.", None),
        ),
        (
            steered,
            message("steered", "Actually", false),
            completed,
            told("", Some(Silence::Steered)),
        ),
        (
            stopped,
            message("stopped", "Never mind", false),
            interrupted,
            told("", Some(Silence::Stopped)),
        ),
        (labelled, opened(Some(label)), TurnState::Running, None),
        (unlabelled, opened(None), TurnState::Running, None),
        (
            queued,
            message("queued", "Still there?", false),
            TurnState::Queued,
            None,
        ),
    ];
    let turns = transcript(&database, "conversation")?;
    assert_eq!(
        read(&turns),
        expected
            .iter()
            .map(|(turn, source, state, answer)| (turn.as_str(), source, *state, &None, answer))
            .collect::<Vec<_>>()
    );
    keeps_back(&turns, &["operator detail"])
}

#[test]
fn a_transcript_pages_back_from_its_newest_turns_within_its_own_conversation() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    let mut written = Vec::new();
    for index in 0..HISTORY_PAGE_LIMIT + 3 {
        written.push(write(
            &database,
            &format!("event-{index}"),
            "conversation",
            "hello",
        )?);
    }
    let elsewhere = write(&database, "elsewhere", "another", "hello")?;
    let foreign = say(&database, "other", &event("foreign", "conversation"))?;
    let page =
        |before: Option<&str>| database.transcript("discord", "agent", "conversation", before);
    let ids = |turns: &[TranscriptTurn]| {
        turns
            .iter()
            .map(|turn| turn.turn_id.clone())
            .collect::<Vec<_>>()
    };
    let newest = page(None)?;
    assert_eq!(ids(&newest.turns), written[3..]);
    assert!(newest.earlier);
    let oldest = page(Some(&newest.turns[0].turn_id))?;
    assert_eq!(ids(&oldest.turns), written[..3]);
    assert!(!oldest.earlier);
    for cursor in [elsewhere.as_str(), foreign.as_str(), "unknown"] {
        assert_eq!(page(Some(cursor))?, Transcript::default(), "{cursor}");
    }
    let others = database.transcript("other", "agent", "conversation", None)?;
    assert_eq!(ids(&others.turns), [foreign]);
    for (agent, audience) in [("agent", "unknown"), ("unknown", "conversation")] {
        let empty = database.transcript("discord", agent, audience, None)?;
        assert_eq!(empty, Transcript::default());
    }
    Ok(())
}
