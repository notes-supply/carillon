use super::*;

#[test]
fn only_a_settled_notice_can_change_the_latest_owner() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    accept(&database, "first", "conversation")?;
    let notice = Admission::Stranded {
        previous_runtime: "runtime",
        explanation: "Your next message starts a new conversation on replacement.",
    };
    let receipt = database.accept_event(
        "discord",
        "replacement",
        &event("notice", "conversation"),
        notice,
        None,
    )?;
    assert_eq!(receipt.state, TurnState::Interrupted);
    assert_eq!(
        database
            .conversation_runtime("discord", "agent", "conversation")?
            .as_deref(),
        Some("replacement")
    );
    crate::assert_refused(
        database.accept_event(
            "discord",
            "third",
            &event("stale", "conversation"),
            notice,
            None,
        ),
        "conversation is owned by runtime replacement, not third",
    )?;
    crate::assert_refused(
        accept(&database, "old", "conversation"),
        "conversation is owned by runtime replacement, not runtime",
    )?;
    assert!(database.event_receipt("discord", "stale")?.is_none());
    assert!(database.event_receipt("discord", "old")?.is_none());
    let queued = database.accept_event(
        "discord",
        "replacement",
        &event("next", "conversation"),
        Admission::Queued {
            providers: &json!([]),
            resource_snapshot_id: "resources",
        },
        None,
    )?;
    assert_eq!(queued.state, TurnState::Queued);
    Ok(())
}

#[test]
fn acceptance_pins_conversations_and_rejects_owner_changes() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    assert_eq!(
        database.conversation_runtime("discord", "agent", "conversation")?,
        None
    );
    accept(&database, "one", "conversation")?;
    assert_eq!(
        database
            .conversation_runtime("discord", "agent", "conversation")?
            .as_deref(),
        Some("runtime")
    );
    assert_eq!(
        database.conversation_runtime("other-interface", "agent", "conversation")?,
        None
    );
    assert_eq!(
        database.conversation_runtime("discord", "other-agent", "conversation")?,
        None
    );
    let second = event("two", "conversation");
    crate::assert_refused(
        database.accept_event(
            "discord",
            "replacement",
            &second,
            Admission::Queued {
                providers: &json!([]),
                resource_snapshot_id: "resources",
            },
            None,
        ),
        "conversation is owned by runtime runtime, not replacement",
    )?;
    assert!(database.event_receipt("discord", "two")?.is_none());
    assert_eq!(database.recent_turns(None, 10)?.len(), 1);
    // A stale runtime selection must not prevent recovery of an existing receipt.
    assert!(
        database
            .accept_event(
                "discord",
                "replacement",
                &event("one", "conversation"),
                Admission::Queued {
                    providers: &json!([]),
                    resource_snapshot_id: "resources",
                },
                None
            )?
            .duplicate
    );
    accept(&database, "two", "conversation")?;
    assert_eq!(database.recent_turns(None, 10)?.len(), 2);
    Ok(())
}

/// A turn still running on the agent's former runtime does not hold back the
/// conversation's messages on its new one.
#[test]
fn a_conversations_messages_wait_only_for_its_own_runtimes_turns() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    accept(&database, "held", "conversation")?;
    let held = database
        .claim("runtime", Lane::Messages, &CancellationToken::new())?
        .context("missing held turn")?;
    let notice = Admission::Stranded {
        previous_runtime: "runtime",
        explanation: "Your next message starts a new conversation on replacement.",
    };
    database.accept_event(
        "discord",
        "replacement",
        &event("notice", "conversation"),
        notice,
        None,
    )?;
    let next = database.accept_event(
        "discord",
        "replacement",
        &event("next", "conversation"),
        Admission::Queued {
            providers: &json!([]),
            resource_snapshot_id: "resources",
        },
        None,
    )?;
    let claimed = database
        .claim("replacement", Lane::Messages, &CancellationToken::new())?
        .context("the other runtime's turn held the message back")?;
    assert_eq!(claimed.turn.turn_id, next.turn_id);
    assert_eq!(
        database
            .inspect_turn(&held.turn.turn_id)?
            .context("missing held turn")?
            .state,
        TurnState::Running
    );
    Ok(())
}
