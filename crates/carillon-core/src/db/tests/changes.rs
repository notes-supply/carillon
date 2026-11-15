use anyhow::ensure;
use carillon_protocol::{
    model::Delivery,
    turns::{Silence, TurnOpening},
};
use tokio::sync::watch;

use super::{super::Opened, *};

/// Fresh storage and what it announces.
struct Watched {
    database: Database,
    changes: watch::Receiver<()>,
    _directory: tempfile::TempDir,
}

impl Watched {
    fn new() -> Result<Self> {
        let directory = tempdir()?;
        let database = Database::create(&directory.path().join("state.sqlite3"))?;
        Ok(Self {
            changes: database.changes(),
            database,
            _directory: directory,
        })
    }

    /// Whether storage announced a change while `act` ran, whatever a case
    /// did to prepare for it.
    fn announces<T>(&mut self, act: impl FnOnce(&Database) -> Result<T>) -> Result<bool> {
        self.changes.mark_unchanged();
        act(&self.database)?;
        Ok(self.changes.has_changed()?)
    }
}

/// Prepares storage, then says whether the step it is named for announced.
type Case = fn(&mut Watched) -> Result<bool>;

const ANSWERED: Settlement<'static> = Settlement::Answered {
    output: "done",
    content: "done",
    activity: None,
    files: &[],
};

const FAILED: Settlement<'static> = Settlement::Failed {
    error: "runtime failed",
    notice: "That turn was interrupted.",
    activity: None,
};

const STRANDED: Admission<'static> = Admission::Stranded {
    previous_runtime: "runtime",
    explanation: "Your next message starts a new conversation.",
};

fn queued(database: &Database) -> Result<QueuedTurn> {
    accept(database, "one", "conversation")?;
    database.next_dispatch("runtime")?.context("nothing queued")
}

fn held(database: &Database) -> Result<QueuedTurn> {
    accept(database, "one", "conversation")?;
    hold(database, "runtime")
}

fn settle(database: &Database, turn: &QueuedTurn, settlement: Settlement<'_>) -> Result<Settled> {
    database.settle_held("runtime", &turn.turn_id, settlement)
}

fn claimed(database: &Database) -> Result<Delivery> {
    let turn = held(database)?;
    answer(database, &turn.turn_id, "done")?;
    database
        .claim_delivery("discord", "instance")?
        .context("nothing to claim")
}

fn acknowledge(database: &Database, delivery: &Delivery, outcome: DeliveryOutcome) -> Result<bool> {
    database.acknowledge_delivery("discord", delivery.id, "instance", outcome, Some("message"))
}

fn open(database: &Database) -> Result<Opened> {
    let opening = TurnOpening {
        interface_id: "discord".to_owned(),
        agent_id: "agent".to_owned(),
        audience_id: "conversation".to_owned(),
        key: "nightly".to_owned(),
        label: None,
    };
    database.open_turn("runtime", Some("runtime"), &opening)
}

const TRANSITIONS: &[(&str, Case)] = &[
    ("a queued admission", |storage| {
        storage.announces(|database| accept(database, "one", "conversation"))
    }),
    ("a stranded admission", |storage| {
        accept(&storage.database, "one", "conversation")?;
        storage.announces(|database| {
            database.accept_event(
                "discord",
                "replacement",
                &event("stranded", "conversation"),
                STRANDED,
                None,
            )
        })
    }),
    ("a hand-over", |storage| {
        let turn = queued(&storage.database)?;
        storage.announces(|database| database.dispatched(&turn))
    }),
    ("an answer", |storage| {
        let turn = held(&storage.database)?;
        storage.announces(|database| settle(database, &turn, ANSWERED))
    }),
    ("a failure", |storage| {
        let turn = held(&storage.database)?;
        storage.announces(|database| settle(database, &turn, FAILED))
    }),
    (
        "a turn ended before its hand-over was recorded",
        |storage| {
            let turn = queued(&storage.database)?;
            storage.announces(|database| settle(database, &turn, FAILED))
        },
    ),
    ("a steered turn", |storage| {
        let turn = held(&storage.database)?;
        storage.announces(|database| settle(database, &turn, Settlement::Silent(Silence::Steered)))
    }),
    ("a stopped turn", |storage| {
        let turn = held(&storage.database)?;
        storage.announces(|database| settle(database, &turn, Settlement::Silent(Silence::Stopped)))
    }),
    ("a turn its runtime opened", |storage| {
        accept(&storage.database, "one", "conversation")?;
        storage.announces(|database| {
            ensure!(matches!(open(database)?, Opened::Receipt(_)));
            Ok(())
        })
    }),
    ("a restart returning a held turn to the queue", |storage| {
        held(&storage.database)?;
        storage.announces(|database| database.recover_running_turns(&["runtime"]))
    }),
    (
        "a restart interrupting a removed runtime's turn",
        |storage| {
            held(&storage.database)?;
            storage.announces(|database| database.recover_running_turns(&[]))
        },
    ),
    (
        "a restart interrupting a moved agent's queued turn",
        |storage| {
            accept(&storage.database, "one", "conversation")?;
            storage.announces(|database| database.interrupt_queued("agent", "runtime", "It moved."))
        },
    ),
    ("a delivery claimed", |storage| {
        let turn = held(&storage.database)?;
        answer(&storage.database, &turn.turn_id, "done")?;
        storage.announces(|database| {
            ensure!(database.claim_delivery("discord", "instance")?.is_some());
            Ok(())
        })
    }),
    ("a delivery acknowledged", |storage| {
        let delivery = claimed(&storage.database)?;
        storage.announces(|database| {
            ensure!(acknowledge(
                database,
                &delivery,
                DeliveryOutcome::Delivered
            )?);
            Ok(())
        })
    }),
    ("an abandoned claim reconciled", |storage| {
        claimed(&storage.database)?;
        storage.announces(|database| database.reconcile_delivery_claims("discord", "next"))
    }),
    ("a write to a table added later", |storage| {
        storage.announces(|database| {
            database.write(|transaction| {
                Ok(transaction.execute_batch(
                    "CREATE TABLE later (note TEXT); INSERT INTO later VALUES ('new')",
                )?)
            })
        })
    }),
];

/// The cases of `cases` that did not announce as `expected`.
fn unlike(cases: &[(&'static str, Case)], expected: bool) -> Result<Vec<&'static str>> {
    let mut unlike = Vec::new();
    for (name, case) in cases {
        if case(&mut Watched::new()?)? != expected {
            unlike.push(*name);
        }
    }
    Ok(unlike)
}

#[test]
fn every_transition_announces_a_change() -> Result<()> {
    let silent = unlike(TRANSITIONS, true)?;
    assert!(silent.is_empty(), "announced nothing: {silent:?}");
    Ok(())
}

const NO_OPS: &[(&str, Case)] = &[
    ("a repeated event", |storage| {
        accept(&storage.database, "one", "conversation")?;
        storage.announces(|database| {
            ensure!(accept(database, "one", "conversation")?.duplicate);
            Ok(())
        })
    }),
    ("a repeated opening", |storage| {
        accept(&storage.database, "one", "conversation")?;
        open(&storage.database)?;
        storage.announces(|database| {
            let Opened::Receipt(opened) = open(database)? else {
                bail!("no turn opened");
            };
            ensure!(opened.receipt.duplicate);
            Ok(())
        })
    }),
    ("an opening in no conversation", |storage| {
        storage.announces(|database| {
            ensure!(matches!(open(database)?, Opened::NoConversation));
            Ok(())
        })
    }),
    ("a hand-over recorded after the turn ended", |storage| {
        let turn = queued(&storage.database)?;
        settle(&storage.database, &turn, ANSWERED)?;
        storage.announces(|database| database.dispatched(&turn))
    }),
    ("a hand-over recorded again", |storage| {
        let turn = held(&storage.database)?;
        storage.announces(|database| database.dispatched(&turn))
    }),
    ("the same outcome again", |storage| {
        let turn = held(&storage.database)?;
        settle(&storage.database, &turn, ANSWERED)?;
        storage.announces(|database| {
            ensure!(settle(database, &turn, ANSWERED)? == Settled::Recorded);
            Ok(())
        })
    }),
    ("another outcome for an ended turn", |storage| {
        let turn = held(&storage.database)?;
        settle(&storage.database, &turn, ANSWERED)?;
        storage.announces(|database| {
            ensure!(settle(database, &turn, FAILED)? == Settled::Superseded);
            Ok(())
        })
    }),
    (
        "an outcome for a turn the runtime does not hold",
        |storage| {
            let turn = held(&storage.database)?;
            storage.announces(|database| {
                let settled = database.settle_held("another", &turn.turn_id, ANSWERED)?;
                ensure!(settled == Settled::Unknown);
                Ok(())
            })
        },
    ),
    ("a restart with nothing to recover", |storage| {
        accept(&storage.database, "one", "conversation")?;
        storage.announces(|database| {
            ensure!(database.recover_running_turns(&["runtime"])? == 0);
            ensure!(database.interrupt_queued("another", "runtime", "It moved.")? == 0);
            Ok(())
        })
    }),
    ("a claim that finds nothing", |storage| {
        held(&storage.database)?;
        storage.announces(|database| {
            ensure!(database.claim_delivery("discord", "instance")?.is_none());
            Ok(())
        })
    }),
    ("a claim its holder repeats", |storage| {
        let delivery = claimed(&storage.database)?;
        storage.announces(|database| {
            let again = database.claim_delivery("discord", "instance")?;
            ensure!(again.is_some_and(|again| again.id == delivery.id));
            Ok(())
        })
    }),
    ("a repeated acknowledgement", |storage| {
        let delivery = claimed(&storage.database)?;
        acknowledge(&storage.database, &delivery, DeliveryOutcome::Delivered)?;
        storage.announces(|database| {
            ensure!(acknowledge(
                database,
                &delivery,
                DeliveryOutcome::Delivered
            )?);
            Ok(())
        })
    }),
    ("an acknowledgement that contradicts the first", |storage| {
        let delivery = claimed(&storage.database)?;
        acknowledge(&storage.database, &delivery, DeliveryOutcome::Delivered)?;
        storage.announces(|database| {
            ensure!(!acknowledge(
                database,
                &delivery,
                DeliveryOutcome::Uncertain
            )?);
            Ok(())
        })
    }),
    ("a reconciliation with no claim to release", |storage| {
        claimed(&storage.database)?;
        storage.announces(|database| database.reconcile_delivery_claims("discord", "instance"))
    }),
    ("a write that fails", |storage| {
        held(&storage.database)?;
        storage.announces(|database| {
            let failed: Result<()> = database.write(|transaction| {
                transaction.execute("UPDATE turns SET state = 'completed'", [])?;
                bail!("the work failed");
            });
            crate::assert_refused(failed, "the work failed")
        })
    }),
];

#[test]
fn a_write_that_changes_nothing_announces_nothing() -> Result<()> {
    let announced = unlike(NO_OPS, false)?;
    assert!(announced.is_empty(), "announced a change: {announced:?}");
    Ok(())
}
