//! What a view shows an interface, as `Core` composes it from storage and
//! from what the runtimes say of the turns that run.

use std::{cell::RefCell, fs, path::Path};

use anyhow::{Context as _, bail};
use carillon_protocol::{
    conversations::TurnSource,
    observation::{
        Conversation, ConversationKey, Ending, Limit, ObservedTurn, OpenView, Opened, Progress,
        Snapshot, View,
    },
    turns::{ControlAction, TurnOpening, TurnOutcome},
};
use tempfile::tempdir;

use super::{Fixture, accept, answer, configured, hand_next, installation, report};
use crate::{HeldTurn, application::http::*};

/// Registers the interface `other-interface`, whose credential is `other-only`.
pub(super) fn second_interface(state: &mut Fixture, directory: &Path) -> Result<()> {
    let mut other = state.config.value.providers[0].clone();
    "other-interface".clone_into(&mut other.id);
    "http://127.0.0.1:9".clone_into(&mut other.url);
    other.token_hash = carillon_installation::config::hash_token("other-only");
    state.config.value.providers.push(other);
    fs::write(
        directory.join("credentials/other-interface.token"),
        "other-only",
    )?;
    state.config.value.validate()
}

/// Accepts a message saying `text` from `other-interface`.
async fn say_elsewhere(state: &Fixture, text: &str, audience: &str) -> Result<EventReceipt> {
    let interface = state
        .config
        .provider("other-interface", ProviderRole::Interface)?;
    let event = ExternalEvent {
        event_id: text.to_owned(),
        agent_id: "test-agent".to_owned(),
        user_id: "someone".to_owned(),
        audience_id: audience.to_owned(),
        channel_id: "other-channel".to_owned(),
        delivery_target_id: "other-destination".to_owned(),
        feedback_target_id: "other-feedback".to_owned(),
        content: text.to_owned(),
        native: Value::Null,
        attachments: None,
    };
    Ok(configured(state)
        .core
        .accept(&selected_provider(interface), &event)
        .await?)
}

pub(super) fn key(audience: &str) -> Result<ConversationKey> {
    Ok(ConversationKey::new(
        "test-agent".to_owned(),
        audience.to_owned(),
    )?)
}

/// A view of `conversations` conversations, with `open`'s newest turns.
pub(super) fn view(conversations: u16, open: Option<(&str, u16)>) -> Result<View> {
    Ok(View {
        conversations: Limit::new(conversations)?,
        open: open
            .map(|(audience, turns)| {
                Ok::<_, anyhow::Error>(OpenView {
                    conversation: key(audience)?,
                    turns: Limit::new(turns)?,
                })
            })
            .transpose()?,
    })
}

/// What `view` shows `interface` while no runtime says anything.
async fn shown(core: &Core, interface: &str, view: &View) -> Result<Snapshot> {
    core.snapshot(interface, view, async |held| vec![None; held.len()])
        .await
}

fn audiences(conversations: &[Conversation]) -> Vec<&str> {
    conversations
        .iter()
        .map(|conversation| conversation.key.audience_id())
        .collect()
}

pub(super) fn recorded(snapshot: &Snapshot) -> Result<(&Conversation, &[ObservedTurn], bool)> {
    match &snapshot.open {
        Some(Opened::Recorded {
            conversation,
            turns,
            earlier,
        }) => Ok((conversation, turns, *earlier)),
        other => bail!("no recorded conversation is open: {other:?}"),
    }
}

/// What each turn said to begin with, in the order shown.
pub(super) fn texts(turns: &[ObservedTurn]) -> Vec<&str> {
    turns
        .iter()
        .map(|turn| match &turn.source {
            TurnSource::Message { text, .. } => text.as_str(),
            TurnSource::Opened { label } => label.as_deref().unwrap_or_default(),
        })
        .collect()
}

/// What a runtime says of a turn at its `calls`th call, offering `controls`.
pub(super) fn live(calls: u32, controls: &[ControlAction]) -> Result<TurnActivity> {
    Ok(serde_json::from_value(json!({
        "phase": "tools",
        "running": [{"tool": "bash", "detail": "cargo test", "started_at": 1}],
        "recent": [],
        "tool_calls": calls,
        "controls": controls,
    }))?)
}

#[tokio::test]
async fn an_interface_is_shown_its_own_conversations_and_never_anothers() -> Result<()> {
    let directory = tempdir()?;
    let mut state = installation(directory.path())?;
    second_interface(&mut state, directory.path())?;
    accept(&state, "mine", "shared").await?;
    accept(&state, "mine alone", "only mine").await?;
    say_elsewhere(&state, "theirs", "shared").await?;
    let core = &configured(&state).core;

    let open = view(50, Some(("shared", 50)))?;
    let mine = shown(core, "test-interface", &open).await?;
    assert_eq!(audiences(&mine.conversations), ["only mine", "shared"]);
    assert_eq!(texts(recorded(&mine)?.1), ["mine"]);
    let theirs = shown(core, "other-interface", &open).await?;
    assert_eq!(audiences(&theirs.conversations), ["shared"]);
    let (conversation, turns, _) = recorded(&theirs)?;
    assert_eq!(conversation.channel_id, "other-channel");
    assert_eq!(texts(turns), ["theirs"]);

    // Another interface's conversation reads as one core has never recorded.
    let other = view(50, Some(("only mine", 50)))?;
    assert_eq!(
        shown(core, "other-interface", &other).await?.open,
        Some(Opened::Unrecorded {
            key: key("only mine")?
        })
    );
    let nobody = shown(core, "unknown-interface", &open).await?;
    assert!(nobody.conversations.is_empty());
    assert_eq!(
        nobody.open,
        Some(Opened::Unrecorded {
            key: key("shared")?
        })
    );
    Ok(())
}

#[tokio::test]
async fn a_view_lists_the_latest_active_conversations_and_the_open_ones_newest_turns() -> Result<()>
{
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let core = &state.core;
    for audience in ["a", "b", "c", "d"] {
        accept(&state, &format!("first in {audience}"), audience).await?;
    }
    let list = async |conversations| -> Result<(Vec<String>, bool)> {
        let snapshot = shown(core, "test-interface", &view(conversations, None)?).await?;
        assert_eq!(snapshot.open, None);
        let listed = audiences(&snapshot.conversations);
        Ok((
            listed.into_iter().map(str::to_owned).collect(),
            snapshot.more,
        ))
    };
    let listing =
        |audiences: &[&str], more| (audiences.iter().map(|&a| a.to_owned()).collect(), more);
    assert_eq!(list(3).await?, listing(&["d", "c", "b"], true));
    assert_eq!(list(4).await?, listing(&["d", "c", "b", "a"], false));
    assert_eq!(list(5).await?, listing(&["d", "c", "b", "a"], false));

    // Ending a turn makes its conversation the latest active, and so does a
    // new message.
    let first = hand_next(core, "test-runtime").await?.context("queued")?;
    answer(core, "test-runtime", &first, None)?;
    assert_eq!(list(2).await?, listing(&["a", "d"], true));
    accept(&state, "second in b", "b").await?;
    accept(&state, "third in b", "b").await?;
    assert_eq!(list(2).await?, listing(&["b", "a"], true));

    // The open conversation is shown whether or not it is listed.
    let newest = shown(core, "test-interface", &view(1, Some(("c", 1)))?).await?;
    assert_eq!(audiences(&newest.conversations), ["b"]);
    let (conversation, turns, earlier) = recorded(&newest)?;
    assert_eq!(
        (conversation.key.audience_id(), conversation.title.as_str()),
        ("c", "first in c")
    );
    assert_eq!((texts(turns), earlier), (vec!["first in c"], false));

    let window = async |turns| -> Result<(Vec<String>, bool)> {
        let snapshot = shown(core, "test-interface", &view(1, Some(("b", turns)))?).await?;
        let (conversation, turns, earlier) = recorded(&snapshot)?;
        assert_eq!(conversation.at_work, 3);
        Ok((
            texts(turns).into_iter().map(str::to_owned).collect(),
            earlier,
        ))
    };
    assert_eq!(
        window(2).await?,
        listing(&["second in b", "third in b"], true)
    );
    let whole = ["first in b", "second in b", "third in b"];
    assert_eq!(window(3).await?, listing(&whole, false));
    assert_eq!(window(4).await?, listing(&whole, false));
    Ok(())
}

#[tokio::test]
async fn a_conversation_reads_unrecorded_until_its_first_message_is_accepted() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let open = view(50, Some(("fresh", 50)))?;
    let before = shown(&state.core, "test-interface", &open).await?;
    assert_eq!(
        before,
        Snapshot {
            conversations: Vec::new(),
            more: false,
            open: Some(Opened::Unrecorded { key: key("fresh")? }),
        }
    );
    let receipt = accept(&state, "hello", "fresh").await?;
    let after = shown(&state.core, "test-interface", &open).await?;
    let (conversation, turns, earlier) = recorded(&after)?;
    assert_eq!(std::slice::from_ref(conversation), after.conversations);
    assert_eq!(
        (
            &conversation.key,
            conversation.channel_id.as_str(),
            conversation.title.as_str(),
            conversation.at_work
        ),
        (&key("fresh")?, "test-channel", "hello", 1)
    );
    let [turn] = turns else {
        bail!("expected one turn: {turns:?}");
    };
    assert_eq!(turn.turn_id, receipt.turn_id);
    assert_eq!(turn.progress, Progress::Queued);
    assert!(!earlier);
    Ok(())
}

#[tokio::test]
async fn each_turn_shows_how_far_it_has_come_and_what_ended_it() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let core = &state.core;
    let outcomes = [
        TurnOutcome::Answered {
            output: "The answer.".to_owned(),
            activity: Some(report(4)?),
            files: Vec::new(),
        },
        TurnOutcome::Failed {
            explanation: Some("The model refused.".to_owned()),
            activity: None,
        },
        TurnOutcome::Steered {},
        TurnOutcome::Stopped {},
    ];
    for (index, outcome) in outcomes.into_iter().enumerate() {
        accept(&state, &format!("message {index}"), "conversation").await?;
        let held = hand_next(core, "test-runtime").await?.context("queued")?;
        core.settle("test-runtime", &held.correlation_id, outcome)?;
    }
    let running = accept(&state, "still running", "conversation").await?;
    hand_next(core, "test-runtime").await?.context("queued")?;
    accept(&state, "still queued", "conversation").await?;

    let asked = RefCell::new(Vec::new());
    let controls = [ControlAction::Stop];
    let snapshot = core
        .snapshot(
            "test-interface",
            &view(1, Some(("conversation", 50)))?,
            async |held| {
                asked.borrow_mut().extend_from_slice(held);
                vec![live(7, &controls).ok()]
            },
        )
        .await?;
    assert_eq!(
        asked.into_inner(),
        [HeldTurn {
            turn_id: running.turn_id,
            runtime_id: "test-runtime".to_owned()
        }]
    );
    let (conversation, turns, _) = recorded(&snapshot)?;
    assert_eq!(conversation.at_work, 2);
    let ended = |turn: &ObservedTurn| match &turn.progress {
        Progress::Ended {
            finished_at,
            ending,
            report,
        } => {
            assert!(*finished_at >= turn.accepted_at);
            Ok((ending.clone(), report.clone()))
        }
        other => bail!("not ended: {other:?}"),
    };
    assert_eq!(
        ended(&turns[0])?,
        (
            Ending::Answered {
                text: "The answer.".to_owned(),
                files: Vec::new()
            },
            Some(report(4)?)
        )
    );
    let (Ending::Interrupted { notice }, None) = ended(&turns[1])? else {
        bail!("not interrupted: {:?}", turns[1]);
    };
    assert!(notice.contains("The model refused."), "{notice}");
    assert_eq!(ended(&turns[2])?, (Ending::Steered, None));
    assert_eq!(ended(&turns[3])?, (Ending::Stopped, None));
    assert_eq!(
        turns[4].progress,
        Progress::Running {
            activity: Some(live(7, &controls)?)
        }
    );
    assert_eq!(turns[5].progress, Progress::Queued);
    assert_eq!(turns.len(), 6);
    Ok(())
}

#[tokio::test]
async fn a_running_turn_whose_runtime_says_nothing_is_still_running() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    accept(&state, "one", "conversation").await?;
    hand_next(&state.core, "test-runtime").await?;
    let snapshot = shown(
        &state.core,
        "test-interface",
        &view(1, Some(("conversation", 50)))?,
    )
    .await?;
    let (conversation, turns, _) = recorded(&snapshot)?;
    assert_eq!(conversation.at_work, 1);
    assert_eq!(turns[0].progress, Progress::Running { activity: None });
    Ok(())
}

#[tokio::test]
async fn a_runtime_is_asked_only_about_the_running_turns_the_view_shows() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let core = &state.core;
    accept(&state, "older", "conversation").await?;
    hand_next(core, "test-runtime").await?;
    accept(&state, "newest", "conversation").await?;
    let newest = hand_next(core, "test-runtime").await?.context("queued")?;
    answer(core, "test-runtime", &newest, None)?;
    accept(&state, "elsewhere", "another").await?;
    hand_next(core, "test-runtime").await?;
    // A window of one shows only the turn that ended, and a listed
    // conversation's turns are not shown at all.
    for (open, running) in [
        (None, 0),
        (Some(("conversation", 1)), 0),
        (Some(("conversation", 2)), 1),
    ] {
        let asked = RefCell::new(0);
        core.snapshot("test-interface", &view(50, open)?, async |held| {
            *asked.borrow_mut() += held.len();
            vec![None; held.len()]
        })
        .await?;
        assert_eq!(asked.into_inner(), running, "{open:?}");
    }
    Ok(())
}

#[tokio::test]
async fn a_turn_the_runtime_opened_shows_its_label_and_offers_no_controls() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    accept(&state, "a message", "conversation").await?;
    hand_next(&state.core, "test-runtime").await?;
    let opening = TurnOpening {
        interface_id: "test-interface".to_owned(),
        agent_id: "test-agent".to_owned(),
        audience_id: "conversation".to_owned(),
        key: "nightly".to_owned(),
        label: Some("The nightly summary.".to_owned()),
    };
    let opened = state.core.open_turn("test-runtime", &opening)?;
    let controls = [ControlAction::Steer, ControlAction::Stop];
    let snapshot = state
        .core
        .snapshot(
            "test-interface",
            &view(1, Some(("conversation", 50)))?,
            async |held| held.iter().map(|_| live(1, &controls).ok()).collect(),
        )
        .await?;
    let (_, turns, _) = recorded(&snapshot)?;
    assert_eq!(texts(turns), ["a message", "The nightly summary."]);
    assert_eq!(
        turns[0].progress,
        Progress::Running {
            activity: Some(live(1, &controls)?)
        }
    );
    assert_eq!(turns[1].turn_id, opened.receipt.turn_id);
    assert_eq!(
        turns[1].source,
        TurnSource::Opened {
            label: opening.label
        }
    );
    assert_eq!(
        turns[1].progress,
        Progress::Running {
            activity: Some(live(1, &[])?)
        }
    );
    Ok(())
}

#[tokio::test]
async fn a_turn_that_ends_while_its_runtime_is_asked_shows_its_stored_ending() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let core = &state.core;
    accept(&state, "one", "conversation").await?;
    let first = hand_next(core, "test-runtime").await?.context("queued")?;
    accept(&state, "two", "conversation").await?;
    let snapshot = core
        .snapshot(
            "test-interface",
            &view(1, Some(("conversation", 50)))?,
            async |held| {
                // The first turn ends and the second starts running before
                // the runtime's answer about the first comes back.
                let ended = answer(core, "test-runtime", &first, report(9).ok());
                let started = hand_next(core, "test-runtime").await;
                assert!(ended.is_ok() && started.is_ok_and(|turn| turn.is_some()));
                held.iter().map(|_| live(3, &[]).ok()).collect()
            },
        )
        .await?;
    let (conversation, turns, _) = recorded(&snapshot)?;
    assert_eq!(conversation.at_work, 1);
    assert_eq!(
        turns[0].progress,
        Progress::Ended {
            finished_at: conversation.active_at,
            ending: Ending::Answered {
                text: "reply to one".to_owned(),
                files: Vec::new()
            },
            report: Some(report(9)?),
        }
    );
    // What was said of the first turn is not shown on the second.
    assert_eq!(turns[1].progress, Progress::Running { activity: None });
    Ok(())
}

#[tokio::test]
async fn a_turn_stored_as_no_turn_can_be_fails_the_snapshot() -> Result<()> {
    let directory = tempdir()?;
    let state = installation(directory.path())?;
    let receipt = accept(&state, "one", "conversation").await?;
    rusqlite::Connection::open(state.config.value.data_dir.join("carillon.sqlite3"))?
        .execute("UPDATE turns SET state = 'completed'", [])?;
    let open = view(1, Some(("conversation", 50)))?;
    crate::assert_refused(
        shown(&state.core, "test-interface", &open).await,
        &format!(
            "turn {} is stored completed with no time it ended and no delivery",
            receipt.turn_id
        ),
    )
}
