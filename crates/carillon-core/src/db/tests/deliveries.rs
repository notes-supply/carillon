use carillon_protocol::turns::{Silence, TurnOpening};

use super::{super::Opened, *};

#[test]
fn delivery_claim_and_ack_retries_are_idempotent() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    accept(&database, "one", "first")?;
    accept(&database, "two", "second")?;
    for output in ["first reply", "second reply"] {
        let turn = hold(&database, "runtime")?;
        answer(&database, &turn.turn_id, output)?;
    }

    let first = database
        .claim_delivery("discord", "instance")?
        .context("first delivery missing")?;
    let retried = database
        .claim_delivery("discord", "instance")?
        .context("retried delivery missing")?;
    assert_eq!(retried.id, first.id);

    assert!(database.acknowledge_delivery(
        "discord",
        first.id,
        "instance",
        DeliveryOutcome::Delivered,
        Some("message"),
    )?);
    assert!(database.acknowledge_delivery(
        "discord",
        first.id,
        "instance",
        DeliveryOutcome::Delivered,
        Some("message"),
    )?);
    assert!(!database.acknowledge_delivery(
        "discord",
        first.id,
        "instance",
        DeliveryOutcome::Uncertain,
        Some("message"),
    )?);
    assert!(!database.acknowledge_delivery(
        "other-interface",
        first.id,
        "instance",
        DeliveryOutcome::Delivered,
        Some("message"),
    )?);

    let second = database
        .claim_delivery("discord", "instance")?
        .context("second delivery missing")?;
    assert_ne!(second.id, first.id);
    Ok(())
}

#[test]
fn a_turns_delivery_is_claimed_without_touching_older_ones() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?;
    let mut turns = Vec::new();
    for (event, output) in [("older", "older reply"), ("wanted", "wanted reply")] {
        accept(&database, event, output)?;
        let turn = hold(&database, "runtime")?;
        answer(&database, &turn.turn_id, output)?;
        turns.push(turn.turn_id);
    }
    let wanted = database
        .claim_turn_delivery("discord", "instance", &turns[1])?
        .context("wanted delivery missing")?;
    assert_eq!(wanted.turn_id, turns[1]);
    // Held by this instance, it is claimed again until acknowledged.
    let again = database
        .claim_turn_delivery("discord", "instance", &turns[1])?
        .context("held delivery missing")?;
    assert_eq!(again.id, wanted.id);
    database.acknowledge_delivery(
        "discord",
        wanted.id,
        "instance",
        DeliveryOutcome::Delivered,
        Some("message"),
    )?;
    assert!(
        database
            .claim_turn_delivery("discord", "instance", &turns[1])?
            .is_none()
    );
    // The older turn's delivery was never claimed: it is still the next.
    let older = database
        .claim_delivery("discord", "other-instance")?
        .context("older delivery missing")?;
    assert_eq!(older.turn_id, turns[0]);
    Ok(())
}

/// The interface every case here submits as.
const INTERFACE: &str = "discord";

/// Every way a turn ends with a delivery row.
#[derive(Clone, Copy, Debug)]
enum Ending {
    Answered,
    /// A runtime's failure, and a hand-over its runtime refused.
    Failed,
    Steered,
    Stopped,
    /// A message for a conversation stranded on a former runtime.
    Stranded,
    /// Queued for an agent that moved, found at startup.
    Moved,
    /// Running on a runtime no longer configured, found at startup.
    Abandoned,
    /// A turn the runtime opened itself.
    Opened,
}

const ENDINGS: [Ending; 8] = [
    Ending::Answered,
    Ending::Failed,
    Ending::Steered,
    Ending::Stopped,
    Ending::Stranded,
    Ending::Moved,
    Ending::Abandoned,
    Ending::Opened,
];

impl Ending {
    /// Ends one turn this way, the only one of `database` with a delivery,
    /// and returns it.
    fn end(self, database: &Database) -> Result<String> {
        let first = accept(database, "first", "conversation")?.turn_id;
        let settle = |turn_id: &str, settlement| -> Result<()> {
            let settled = database.settle_held("runtime", turn_id, settlement)?;
            assert_eq!(settled, Settled::Recorded);
            Ok(())
        };
        match self {
            Self::Answered => answer(database, &hold(database, "runtime")?.turn_id, "done")?,
            Self::Failed => settle(
                &first,
                Settlement::Failed {
                    error: "runtime failed",
                    notice: "That turn was interrupted.",
                    activity: None,
                },
            )?,
            Self::Steered => settle(&first, Settlement::Silent(Silence::Steered))?,
            Self::Stopped => settle(&first, Settlement::Silent(Silence::Stopped))?,
            Self::Stranded => {
                let notice = Admission::Stranded {
                    previous_runtime: "runtime",
                    explanation: "Your next message starts a new conversation.",
                };
                let event = event("stranded", "conversation");
                return Ok(database
                    .accept_event(INTERFACE, "replacement", &event, notice, None)?
                    .turn_id);
            }
            Self::Moved => {
                assert_eq!(
                    database.interrupt_queued("agent", "runtime", "It moved.")?,
                    1
                );
            }
            Self::Abandoned => {
                hold(database, "runtime")?;
                assert_eq!(database.recover_running_turns(&[])?, 1);
            }
            Self::Opened => {
                let opening = TurnOpening {
                    interface_id: INTERFACE.to_owned(),
                    agent_id: "agent".to_owned(),
                    audience_id: "conversation".to_owned(),
                    key: "nightly".to_owned(),
                    label: None,
                };
                let Opened::Receipt(opened) =
                    database.open_turn("runtime", Some("runtime"), &opening)?
                else {
                    bail!("no turn opened");
                };
                answer(database, &opened.receipt.turn_id, "report")?;
                return Ok(opened.receipt.turn_id);
            }
        }
        Ok(first)
    }

    const fn silent(self) -> bool {
        matches!(self, Self::Steered | Self::Stopped)
    }
}

fn delivery_state(database: &Database, turn_id: &str) -> Result<Option<DeliveryState>> {
    Ok(database
        .inspect_turn(turn_id)?
        .context("missing turn")?
        .delivery_state)
}

#[test]
fn every_ending_of_a_retained_interfaces_turn_is_delivered_as_it_is_recorded() -> Result<()> {
    for ending in ENDINGS {
        let directory = tempdir()?;
        let database = Database::create(&directory.path().join("state.sqlite3"))?
            .retaining([INTERFACE.to_owned()].into());
        let turn = ending.end(&database)?;
        assert_eq!(
            delivery_state(&database, &turn)?,
            Some(DeliveryState::Delivered),
            "{ending:?}"
        );
        let counts = database.delivery_counts()?;
        assert_eq!(
            (
                counts.pending,
                counts.active,
                counts.completed,
                counts.uncertain
            ),
            (0, 0, 1, 0),
            "{ending:?}"
        );
        assert_eq!(database.undelivered(&[INTERFACE])?, 0, "{ending:?}");
        assert!(database.claim_delivery(INTERFACE, "instance")?.is_none());
        assert!(
            database
                .claim_turn_delivery(INTERFACE, "instance", &turn)?
                .is_none()
        );
        assert!(
            database
                .reconcile_delivery_claims(INTERFACE, "next")?
                .is_empty()
        );
        let id: i64 = database.connect()?.query_row(
            "SELECT id FROM deliveries WHERE turn_id = ?1",
            [&turn],
            |row| row.get(0),
        )?;
        assert!(!database.acknowledge_delivery(
            INTERFACE,
            id,
            "instance",
            DeliveryOutcome::Uncertain,
            None
        )?);
        assert_eq!(
            delivery_state(&database, &turn)?,
            Some(DeliveryState::Delivered),
            "{ending:?}"
        );
    }
    Ok(())
}

#[test]
fn every_ending_of_an_external_interfaces_turn_waits_for_its_claim() -> Result<()> {
    for ending in ENDINGS {
        let directory = tempdir()?;
        let database = Database::create(&directory.path().join("state.sqlite3"))?
            .retaining(["web".to_owned()].into());
        let turn = ending.end(&database)?;
        assert_eq!(
            delivery_state(&database, &turn)?,
            Some(DeliveryState::Pending),
            "{ending:?}"
        );
        assert_eq!(database.undelivered(&[INTERFACE])?, 1, "{ending:?}");
        let claimed = database
            .claim_delivery(INTERFACE, "dead")?
            .with_context(|| format!("{ending:?} left nothing to claim"))?;
        assert_eq!(claimed.turn_id, turn);
        assert_eq!(database.undelivered(&[INTERFACE])?, 1, "{ending:?}");
        let uncertain = database.reconcile_delivery_claims(INTERFACE, "next")?;
        if ending.silent() {
            assert!(uncertain.is_empty(), "{ending:?}");
            let again = database
                .claim_delivery(INTERFACE, "next")?
                .with_context(|| format!("{ending:?} was not offered again"))?;
            assert_eq!(again.id, claimed.id);
            continue;
        }
        let marked: &[&str] = if matches!(ending, Ending::Opened) {
            &[]
        } else {
            &["message"]
        };
        assert_eq!(uncertain, marked, "{ending:?}");
        assert_eq!(
            delivery_state(&database, &turn)?,
            Some(DeliveryState::Uncertain),
            "{ending:?}"
        );
        assert!(database.claim_delivery(INTERFACE, "next")?.is_none());
        assert_eq!(database.undelivered(&[INTERFACE])?, 0, "{ending:?}");
    }
    Ok(())
}

#[test]
fn a_retained_answer_commits_with_its_turns_end_or_not_at_all() -> Result<()> {
    let directory = tempdir()?;
    let database = Database::create(&directory.path().join("state.sqlite3"))?
        .retaining([INTERFACE.to_owned()].into());
    accept(&database, "one", "conversation")?;
    let held = hold(&database, "runtime")?;
    database.connect()?.execute_batch(
        "CREATE TRIGGER refuse_deliveries BEFORE INSERT ON deliveries
         BEGIN SELECT RAISE(ABORT, 'no delivery'); END",
    )?;
    let answered = Settlement::Answered {
        output: "done",
        content: "done",
        activity: None,
        files: &[],
    };
    crate::assert_refused(
        database.settle_held("runtime", &held.turn_id, answered),
        "no delivery",
    )?;
    let turn = database
        .inspect_turn(&held.turn_id)?
        .context("missing turn")?;
    assert_eq!((turn.state, turn.output), (TurnState::Running, None));
    assert_eq!(database.delivery_counts()?.completed, 0);
    database
        .connect()?
        .execute_batch("DROP TRIGGER refuse_deliveries")?;
    answer(&database, &held.turn_id, "done")?;
    assert_eq!(
        delivery_state(&database, &held.turn_id)?,
        Some(DeliveryState::Delivered)
    );
    Ok(())
}
