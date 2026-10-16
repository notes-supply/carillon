//! The operator's pause of the runtime workers, against the real supervisor,
//! storage and a runtime whose turns wait for the case: what settles, what
//! waits, which arrangement a report may rely on, and how long the pause
//! is known to hold while the services stop.

use std::{
    fs,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use anyhow::Context as _;
use carillon_protocol::model::DeliveryOutcome;
use tempfile::tempdir;
use uuid::Uuid;

use super::{
    Fixture,
    live::{HeldRuntime, apply, counts, edit, lock, ok, operator, serving, submit, workers_are},
};
use crate::application::{
    drain::{Held, Pace, drain},
    http::{
        maintenance::{self, RuntimeRestart},
        *,
    },
};

async fn begin(state: &AppState, lease: Duration) -> Result<Drain, ApiError> {
    let lease = Lease {
        lease_ms: u64::try_from(lease.as_millis()).map_err(anyhow::Error::from)?,
    };
    maintenance::begin(State(state.clone()), operator()?, Json(lease))
        .await
        .map(|Json(drain)| drain)
}

async fn renew(state: &AppState, holder: Uuid) -> Result<Drain, ApiError> {
    maintenance::renew(State(state.clone()), operator()?, AxumPath(holder))
        .await
        .map(|Json(drain)| drain)
}

async fn end(state: &AppState, holder: Uuid) -> Result<StatusCode, ApiError> {
    maintenance::end(State(state.clone()), operator()?, AxumPath(holder)).await
}

/// Renews the pause until its drain is `wanted`.
async fn until(state: &AppState, holder: Uuid, wanted: impl Fn(&Drain) -> bool) -> Result<Drain> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let drain = ok(renew(state, holder).await)?;
            if wanted(&drain) {
                return Ok(drain);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("the drain never stood as wanted")?
}

fn supervise(state: &AppState) -> (CancellationToken, tokio::task::JoinHandle<()>) {
    let stopping = CancellationToken::new();
    let supervisor = tokio::spawn(workers::supervise(
        state.clone(),
        state.workers.clone(),
        stopping.clone(),
    ));
    (stopping, supervisor)
}

fn ready() -> Vec<RuntimeRestart> {
    vec![RuntimeRestart {
        id: "test-runtime".to_owned(),
        unready: None,
    }]
}

/// The interface delivers every answer waiting for it, as it does while it
/// runs.
fn deliver(state: &AppState) -> Result<usize> {
    let mut delivered = 0;
    while let Some(delivery) = state.core.claim_delivery("test-interface", "instance")? {
        let ack = DeliveryAck {
            native_ref: None,
            outcome: DeliveryOutcome::Delivered,
        };
        state
            .core
            .acknowledge_delivery("test-interface", "instance", delivery.id, &ack)
            .map_err(|error| anyhow!("{error:?}"))?;
        delivered += 1;
    }
    Ok(delivered)
}

#[tokio::test]
async fn a_pause_lets_a_held_turn_settle_and_be_delivered_while_queued_work_waits() -> Result<()> {
    let directory = tempdir()?;
    let runtime = HeldRuntime::start().await?;
    let (_fixture, state) = serving(directory.path(), &runtime, 1)?;
    ok(submit(&state, "busy", "one").await)?;
    ok(submit(&state, "next", "two").await)?;
    let (stopping, supervisor) = supervise(&state);
    runtime.holding(1).await?;
    let holder = ok(begin(&state, Duration::from_secs(30)).await)?.holder;
    // The busy turn is neither cancelled nor replayed: its worker counts
    // until it ends, and no runtime is asked about a restart meanwhile.
    let drain = until(&state, holder, |drain| drain.workers == Some(1)).await?;
    assert!(drain.runtimes.is_empty());
    // Events are accepted while paused, and wait.
    ok(submit(&state, "during", "three").await)?;
    runtime.release("busy", None)?;
    let drain = until(&state, holder, |drain| drain.workers == Some(0)).await?;
    assert_eq!(drain.undelivered, 1);
    assert_eq!(drain.runtimes, ready());
    // Its answer is delivered while the interface still runs, once.
    assert_eq!(deliver(&state)?, 1);
    let drain = ok(renew(&state, holder).await)?;
    assert_eq!((drain.workers, drain.undelivered), (Some(0), 0));
    assert_eq!(runtime.given(), [("busy".to_owned(), false)]);
    assert_eq!(counts(&state)?, (2, 0, 1));
    // Once the pause ends the queued work runs, each turn once.
    assert_eq!(ok(end(&state, holder).await)?, StatusCode::NO_CONTENT);
    runtime.drain(&state, (0, 0, 3)).await?;
    let mut given = runtime.given();
    given.sort();
    assert_eq!(
        given,
        ["busy", "during", "next"].map(|input| (input.to_owned(), false))
    );
    assert_eq!(deliver(&state)?, 2);
    stopping.cancel();
    supervisor.await?;
    Ok(())
}

#[tokio::test]
async fn an_observation_is_set_aside_and_asked_again_without_replaying_its_message() -> Result<()> {
    let directory = tempdir()?;
    let runtime = HeldRuntime::start().await?;
    let (_fixture, state) = serving(directory.path(), &runtime, 1)?;
    ok(submit(&state, "first", "one").await)?;
    let (stopping, supervisor) = supervise(&state);
    runtime.holding(1).await?;
    runtime.release("first", Some(json!({"cursor": 1})))?;
    // The observation of the delegated work waits on the runtime.
    runtime.holding(1).await?;
    let holder = ok(begin(&state, Duration::from_secs(30)).await)?.holder;
    // Core stops waiting for it and queues it again through its outcome,
    // rather than holding maintenance for as long as the work takes.
    until(&state, holder, |drain| drain.workers == Some(0)).await?;
    assert_eq!(counts(&state)?, (1, 0, 1));
    ok(end(&state, holder).await)?;
    // Asked again once the pause ends; the message is not run again.
    tokio::time::timeout(Duration::from_secs(10), async {
        while runtime.given().len() < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    runtime.release("", None)?;
    runtime.drain(&state, (0, 0, 2)).await?;
    assert_eq!(
        runtime.given(),
        [
            ("first".to_owned(), false),
            (String::new(), true),
            (String::new(), true)
        ]
    );
    stopping.cancel();
    supervisor.await?;
    Ok(())
}

/// Holds the database writer, so a claim waits for it.
fn hold_writer(fixture: &Fixture) -> Result<rusqlite::Connection> {
    let writer =
        rusqlite::Connection::open(fixture.config.value.data_dir.join("carillon.sqlite3"))?;
    writer.execute_batch("BEGIN IMMEDIATE")?;
    Ok(writer)
}

#[tokio::test]
async fn a_blocked_claim_holds_the_barrier_and_no_holder_reads_another_arrangement() -> Result<()> {
    let directory = tempdir()?;
    let runtime = HeldRuntime::start().await?;
    let (mut fixture, state) = serving(directory.path(), &runtime, 1)?;
    ok(submit(&state, "a1", "a").await)?;
    ok(submit(&state, "b1", "b").await)?;
    // Every worker's claim waits for the database writer.
    let writer = hold_writer(&fixture)?;
    let (stopping, supervisor) = supervise(&state);
    workers_are(&state, |runtime| {
        runtime.messages.claiming == 1 && runtime.observations.claiming == 4
    })
    .await?;
    let lease = Duration::from_millis(1500);
    let first = ok(begin(&state, lease).await)?.holder;
    // Retired, they have not ended while their claims wait: the barrier
    // holds however the rows stand.
    workers_are(&state, |runtime| {
        runtime.messages.claiming + runtime.observations.claiming == 0
    })
    .await?;
    for _ in 0..3 {
        let drain = ok(renew(&state, first).await)?;
        assert!(
            drain.workers.is_some_and(|workers| workers > 0),
            "{drain:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    writer.execute_batch("ROLLBACK")?;
    until(&state, first, |drain| drain.workers == Some(0)).await?;
    assert_eq!(counts(&state)?, (2, 0, 0));

    // The first pause lapses and another holder begins before the
    // supervisor has seen it: the drained arrangement was the first's, and
    // tells the second nothing.
    std::thread::sleep(lease + Duration::from_millis(100));
    let second = ok(begin(&state, Duration::from_secs(30)).await)?;
    assert_eq!(second.workers, None);
    assert!(ok(renew(&state, first).await.map(|_| ())).is_err());

    // Ended, the workers return and wait on the writer again; a third
    // holder is never told they ended while they have not.
    let writer = hold_writer(&fixture)?;
    ok(end(&state, second.holder).await)?;
    workers_are(&state, |runtime| {
        runtime.messages.claiming == 1 && runtime.observations.claiming == 4
    })
    .await?;
    let third = ok(begin(&state, Duration::from_secs(30)).await)?.holder;
    // A registry applied meanwhile is served, and starts no worker.
    edit(&mut fixture, |provider| provider.parallel_turns = Some(2))?;
    let applied = ok(apply(&state).await)?;
    assert!(applied.paused);
    assert_eq!(applied.runtimes[0].workers.messages.claiming, 0);
    for _ in 0..3 {
        let drain = ok(renew(&state, third).await)?;
        assert_ne!(drain.workers, Some(0), "{drain:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    writer.execute_batch("ROLLBACK")?;
    until(&state, third, |drain| drain.workers == Some(0)).await?;
    assert_eq!(counts(&state)?, (2, 0, 0));
    assert!(runtime.given().is_empty());
    // Once it ends, the workers follow the applied registry.
    ok(end(&state, third).await)?;
    runtime.holding(2).await?;
    stopping.cancel();
    for input in ["a1", "b1"] {
        runtime.release(input, None)?;
    }
    supervisor.await?;
    Ok(())
}

#[tokio::test]
async fn one_holder_at_a_time_renews_only_a_pause_still_in_force() -> Result<()> {
    let directory = tempdir()?;
    let runtime = HeldRuntime::start().await?;
    let (_fixture, state) = serving(directory.path(), &runtime, 1)?;
    let (stopping, supervisor) = supervise(&state);
    // Only the operator pauses, for a bounded lease.
    let refused = maintenance::begin(
        State(state.clone()),
        HeaderMap::new(),
        Json(Lease { lease_ms: 1000 }),
    )
    .await
    .err()
    .context("paused without the operator")?;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    for lease in [Duration::ZERO, Duration::from_secs(61)] {
        let refused = begin(&state, lease)
            .await
            .err()
            .context("unbounded lease")?;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    }
    let first = ok(begin(&state, Duration::from_millis(300)).await)?.holder;
    let refused = begin(&state, Duration::from_secs(30))
        .await
        .err()
        .context("two holders")?;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    // Unrenewed, it lapses: the workers return, and it is not renewed.
    tokio::time::sleep(Duration::from_millis(400)).await;
    workers_are(&state, |runtime| runtime.observations.claiming == 4).await?;
    assert_eq!(state.workers.borrow().paused, None);
    let refused = renew(&state, first)
        .await
        .err()
        .context("renewed a lapse")?;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    let second = ok(begin(&state, Duration::from_secs(30)).await)?.holder;
    // An earlier holder's end does not end another's pause.
    ok(end(&state, first).await)?;
    let drain = until(&state, second, |drain| drain.workers == Some(0)).await?;
    assert_eq!(drain.runtimes, ready());
    // A runtime's own refusal is reported; an answer that is not a
    // readiness never counts as ready.
    *lock(&runtime.restart) = json!({"state": "unready", "reason": "a fence"});
    assert_eq!(
        ok(renew(&state, second).await)?.runtimes[0]
            .unready
            .as_deref(),
        Some("a fence")
    );
    for answer in [
        json!({"state": "ready", "also": 1}),
        json!({"state": "maybe"}),
    ] {
        *lock(&runtime.restart) = answer;
        let drain = ok(renew(&state, second).await)?;
        let unready = drain.runtimes[0].unready.as_deref().unwrap_or_default();
        assert!(unready.starts_with("it did not say"), "{drain:?}");
    }
    ok(end(&state, second).await)?;
    let refused = renew(&state, second)
        .await
        .err()
        .context("renewed an ended pause")?;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    stopping.cancel();
    supervisor.await?;
    Ok(())
}

/// Core as one run of its service: its operator API at an address of its
/// own, and its workers.
struct Running {
    config: LoadedConfig,
    /// Ends the API alone, as if core stopped answering while it runs.
    http: CancellationToken,
    stopping: CancellationToken,
    supervisor: tokio::task::JoinHandle<()>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Running {
    async fn start(fixture: &Fixture, state: &AppState) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let mut config = fixture.config.clone();
        config.value.core.listen = listener.local_addr()?;
        fs::write(
            config.root.join("credentials/operator.token"),
            "test-operator",
        )?;
        let http = CancellationToken::new();
        let server = tokio::spawn(
            axum::serve(listener, router(state.clone()))
                .with_graceful_shutdown(http.clone().cancelled_owned())
                .into_future(),
        );
        let (stopping, supervisor) = supervise(state);
        Ok(Self {
            config,
            http,
            stopping,
            supervisor,
            server,
        })
    }

    /// Stops as core's service does, until the service manager would
    /// report it stopped: it answers nothing more, claims nothing more and
    /// settles its workers.
    async fn stop(self) -> Result<()> {
        self.http.cancel();
        self.stopping.cancel();
        self.supervisor.await?;
        self.server.await??;
        Ok(())
    }
}

const PACE: Pace = Pace {
    lease: Duration::from_millis(600),
    renew: Duration::from_millis(150),
    check: Duration::from_millis(50),
    request: Duration::from_millis(400),
};

#[tokio::test]
async fn the_cli_stops_nothing_on_a_drain_it_cannot_show_and_the_workers_return() -> Result<()> {
    let directory = tempdir()?;
    let runtime = HeldRuntime::start().await?;
    let (fixture, state) = serving(directory.path(), &runtime, 1)?;
    ok(submit(&state, "busy", "one").await)?;
    let running = Running::start(&fixture, &state).await?;
    runtime.holding(1).await?;
    let timeout = Duration::from_secs(1);
    let refusal = |error: Option<anyhow::Error>| -> Result<String> {
        Ok(format!("{:#}", error.context("drained")?))
    };
    // A refusal at the deadline asks nothing more of core: its pause lapses
    // within the lease and the workers return.
    let lapsed = || async {
        let mut census = state.workers.subscribe();
        tokio::time::timeout(
            PACE.lease * 4,
            census.wait_for(|census| census.paused.is_none()),
        )
        .await
        .context("the pause did not lapse")?
        .map(|_| ())
        .map_err(anyhow::Error::from)
    };
    // A turn still running: refused once the time is up.
    let refused = refusal(drain(&running.config, timeout, false, PACE).await.err())?;
    assert!(
        refused.contains("did not drain within 1 seconds"),
        "{refused}"
    );
    assert!(
        refused.contains("1 runtime worker(s) have not ended"),
        "{refused}"
    );
    lapsed().await?;
    workers_are(&state, |runtime| runtime.observations.claiming == 4).await?;
    // An answer its interface has not delivered.
    runtime.release("busy", None)?;
    runtime.drain(&state, (0, 0, 1)).await?;
    let refused = refusal(drain(&running.config, timeout, false, PACE).await.err())?;
    assert!(
        refused.contains("1 answer(s) not yet delivered"),
        "{refused}"
    );
    lapsed().await?;
    deliver(&state)?;
    // A runtime that would lose what it holds.
    *lock(&runtime.restart) = json!({"state": "unready", "reason": "a fence"});
    let refused = refusal(drain(&running.config, timeout, false, PACE).await.err())?;
    assert!(
        refused.contains("test-runtime is not ready to restart: a fence"),
        "{refused}"
    );
    lapsed().await?;
    // Forced, it pauses and does not wait.
    let forced = drain(&running.config, timeout, true, PACE).await?;
    forced
        .context("forced without a pause")?
        .end(Duration::from_secs(2))
        .await;
    *lock(&runtime.restart) = json!({"state": "ready"});
    drain(&running.config, timeout, false, PACE)
        .await?
        .context("drained without a pause")?
        .end(Duration::from_secs(2))
        .await;
    // Core that does not answer shows no drain; forced, nothing is paused.
    let config = running.config.clone();
    running.stop().await?;
    let refused = refusal(drain(&config, timeout, false, PACE).await.err())?;
    assert!(
        refused.contains("did not pause its runtime workers"),
        "{refused}"
    );
    assert!(drain(&config, timeout, true, PACE).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn the_pause_holds_through_a_slow_stop_only_while_core_acknowledges_it() -> Result<()> {
    let directory = tempdir()?;
    let runtime = HeldRuntime::start().await?;
    let (fixture, state) = serving(directory.path(), &runtime, 1)?;
    let slow = PACE.lease * 3;

    // Renewed throughout a stop three leases long: work accepted meanwhile
    // waits, and core is seen to stop within a lease of the last renewal.
    let running = Running::start(&fixture, &state).await?;
    let mut hold = drain(&running.config, Duration::from_secs(5), false, PACE)
        .await?
        .context("no pause")?;
    ok(submit(&state, "first", "one").await)?;
    let (stopped, kept) = hold
        .through(async {
            tokio::time::sleep(slow).await;
            running.stop().await
        })
        .await;
    stopped?;
    assert_eq!(kept, Held::Throughout);
    assert!(runtime.given().is_empty());

    // The next run answers the message that waited, and delivers it.
    state.pause.send_replace(None);
    let running = Running::start(&fixture, &state).await?;
    runtime.holding(1).await?;
    runtime.release("first", None)?;
    runtime.drain(&state, (0, 0, 1)).await?;
    deliver(&state)?;
    // Core stops answering while it runs: renewals fail without a refusal,
    // the pause lapses unseen and its workers claim what waits. That is no
    // evidence the pause held.
    let mut hold = drain(&running.config, Duration::from_secs(5), false, PACE)
        .await?
        .context("no pause")?;
    ok(submit(&state, "second", "two").await)?;
    let (stopped, kept) = hold
        .through(async {
            running.http.cancel();
            runtime.holding(1).await?;
            runtime.release("second", None)?;
            running.stop().await
        })
        .await;
    stopped?;
    assert_eq!(kept, Held::Unconfirmed);
    assert_eq!(
        runtime.given(),
        ["first", "second"].map(|input| (input.to_owned(), false))
    );

    // Core says the pause no longer holds.
    state.pause.send_replace(None);
    deliver(&state)?;
    let running = Running::start(&fixture, &state).await?;
    let mut hold = drain(&running.config, Duration::from_secs(5), false, PACE)
        .await?
        .context("no pause")?;
    let holder = state.pause.borrow().context("no pause")?.holder;
    let (stopped, kept) = hold
        .through(async {
            ok(end(&state, holder).await)?;
            tokio::time::sleep(slow).await;
            running.stop().await
        })
        .await;
    stopped?;
    assert_eq!(kept, Held::Lapsed);
    Ok(())
}

/// Core serving with its runtime disabled, so no worker runs and nothing
/// but the pause changes the arrangement; its runtime answers `/v1/restart`
/// after `delay`.
async fn without_workers(
    directory: &std::path::Path,
    delay: Duration,
) -> Result<(Fixture, AppState)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let readiness = Router::new().route(
        "/v1/restart",
        get(move || async move {
            tokio::time::sleep(delay).await;
            Json(json!({"state": "ready"}))
        }),
    );
    tokio::spawn(async move { axum::serve(listener, readiness).await });
    let mut fixture = super::installation(directory)?;
    let runtime = &mut fixture.config.value.providers[1];
    runtime.url = url;
    runtime.enabled = false;
    let state = super::configured(&fixture);
    Ok((fixture, state))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pause_arranged_just_before_it_lapses_still_ends() -> Result<()> {
    let directory = tempdir()?;
    let (_fixture, state) = without_workers(directory.path(), Duration::ZERO).await?;
    let (stopping, supervisor) = supervise(&state);
    let mut census = state.workers.subscribe();
    census.wait_for(|census| census.registry.is_some()).await?;
    // A reader of the census holds it, so the supervisor, having arranged
    // the workers under the pause while it was in force, publishes that
    // only after the pause has lapsed. No other event follows.
    let reading = Arc::new(std::sync::Barrier::new(2));
    let reader = std::thread::spawn({
        let (state, reading) = (state.clone(), reading.clone());
        move || {
            let _census = state.workers.borrow();
            reading.wait();
            std::thread::sleep(Duration::from_millis(400));
        }
    });
    reading.wait();
    // Its report may come after the lapse, and then says so.
    let _ = begin(&state, Duration::from_millis(100)).await;
    let holder = state.pause.borrow().context("no pause")?.holder;
    reader
        .join()
        .map_err(|_| anyhow!("the census reader panicked"))?;
    // The supervisor still ends the pause it was arranged under.
    tokio::time::timeout(
        Duration::from_secs(5),
        census.wait_for(|census| census.paused.is_none()),
    )
    .await
    .with_context(|| format!("still paused for {holder}"))??;
    stopping.cancel();
    supervisor.await?;
    Ok(())
}

#[tokio::test]
async fn a_drain_report_outlived_by_its_pause_is_not_given() -> Result<()> {
    let directory = tempdir()?;
    // The runtime answers only after the pause's lease.
    let (_fixture, state) = without_workers(directory.path(), Duration::from_millis(500)).await?;
    let (stopping, supervisor) = supervise(&state);
    let holder = ok(begin(&state, Duration::from_millis(300)).await)?.holder;
    let mut census = state.workers.subscribe();
    census
        .wait_for(|census| census.paused == Some(holder))
        .await?;
    // Asked once no worker remains, the runtime says it is ready, but by
    // then the pause has lapsed: that is what is reported.
    let refused = renew(&state, holder)
        .await
        .err()
        .context("a lapsed pause was reported drained")?;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    stopping.cancel();
    supervisor.await?;
    Ok(())
}

#[tokio::test]
async fn the_timeout_bounds_the_whole_drain_however_slowly_core_answers() -> Result<()> {
    let directory = tempdir()?;
    // A core that takes the connection and never answers, and a real core
    // whose runtime takes longer to say it can restart than the timeout.
    let (fixture, state) = without_workers(directory.path(), Duration::from_secs(5)).await?;
    let silent = TcpListener::bind("127.0.0.1:0").await?;
    let mut hung = fixture.config.clone();
    hung.value.core.listen = silent.local_addr()?;
    let held = tokio::spawn(async move {
        #[expect(
            clippy::collection_is_never_read,
            reason = "held so core's requests stay open and unanswered"
        )]
        let mut connections = Vec::new();
        while let Ok((connection, _)) = silent.accept().await {
            connections.push(connection);
        }
    });
    let running = Running::start(&fixture, &state).await?;
    for (config, timeout) in [
        (&hung, Duration::ZERO),
        (&hung, Duration::from_secs(1)),
        (&running.config, Duration::from_secs(1)),
    ] {
        state.pause.send_replace(None);
        let started = tokio::time::Instant::now();
        // Each request may wait longer than the whole drain here.
        let pace = Pace {
            request: Duration::from_secs(5),
            ..PACE
        };
        let refused = drain(config, timeout, false, pace)
            .await
            .err()
            .context("drained past its timeout")?;
        let elapsed = started.elapsed();
        let refused = format!("{refused:#}");
        assert!(
            elapsed < timeout + Duration::from_millis(800),
            "{elapsed:?} for {timeout:?}: {refused}"
        );
        assert!(
            refused.contains(&format!(
                "did not drain within {} seconds",
                timeout.as_secs()
            )),
            "{refused}"
        );
    }
    held.abort();
    running.stop().await
}

/// A core whose pause requests the test controls: the report never drains,
/// a renewal is refused while `refuse` is set, and an end is counted and
/// never answered.
async fn undrained_core(
    fixture: &Fixture,
) -> Result<(LoadedConfig, Arc<AtomicBool>, Arc<AtomicUsize>)> {
    let (refuse, ends) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicUsize::new(0)),
    );
    let report = Drain {
        holder: Uuid::now_v7(),
        workers: Some(1),
        undelivered: 0,
        runtimes: Vec::new(),
    };
    let renewed = (report.clone(), refuse.clone());
    let ended = ends.clone();
    let router = Router::new()
        .route("/v1/pause", post(move || async move { Json(report) }))
        .route(
            "/v1/pause/{holder}",
            put(move || async move {
                if renewed.1.load(Ordering::SeqCst) {
                    (
                        StatusCode::CONFLICT,
                        Json(json!({"error": "not this pause"})),
                    )
                        .into_response()
                } else {
                    Json(renewed.0).into_response()
                }
            })
            .delete(move || async move {
                ended.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let mut config = fixture.config.clone();
    config.value.core.listen = listener.local_addr()?;
    fs::write(
        config.root.join("credentials/operator.token"),
        "test-operator",
    )?;
    tokio::spawn(async move { axum::serve(listener, router).await });
    Ok((config, refuse, ends))
}

#[tokio::test]
async fn forcing_waits_for_one_request_to_core_not_the_whole_timeout() -> Result<()> {
    let directory = tempdir()?;
    let fixture = super::installation(directory.path())?;
    let silent = TcpListener::bind("127.0.0.1:0").await?;
    let mut config = fixture.config.clone();
    config.value.core.listen = silent.local_addr()?;
    fs::write(
        config.root.join("credentials/operator.token"),
        "test-operator",
    )?;
    let held = tokio::spawn(async move {
        #[expect(
            clippy::collection_is_never_read,
            reason = "held so core's requests stay open and unanswered"
        )]
        let mut connections = Vec::new();
        while let Ok((connection, _)) = silent.accept().await {
            connections.push(connection);
        }
    });
    for force in [true, false] {
        let started = tokio::time::Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            drain(&config, Duration::from_secs(60), force, PACE),
        )
        .await
        .context("a core that never answers held the drain past one request")?;
        assert!(started.elapsed() < PACE.request + Duration::from_millis(500));
        if force {
            assert!(
                outcome?.is_none(),
                "forced on with a pause core never granted"
            );
        } else {
            let refused = format!("{:#}", outcome.err().context("drained a silent core")?);
            assert!(
                refused.contains("did not pause its runtime workers"),
                "{refused}"
            );
        }
    }
    held.abort();
    Ok(())
}

#[tokio::test]
async fn ending_a_refused_drain_stays_within_the_timeout() -> Result<()> {
    let directory = tempdir()?;
    let fixture = super::installation(directory.path())?;
    let (config, refuse, ends) = undrained_core(&fixture).await?;
    // The time is spent: the pause is left to lapse, and nothing more is asked.
    let timeout = Duration::from_secs(1);
    let started = tokio::time::Instant::now();
    let refused = drain(&config, timeout, false, PACE)
        .await
        .err()
        .context("an undrained core drained")?;
    assert!(
        started.elapsed() < timeout + Duration::from_millis(300),
        "{:?}: {refused:#}",
        started.elapsed()
    );
    assert!(format!("{refused:#}").contains("did not drain within 1 seconds"));
    assert_eq!(ends.load(Ordering::SeqCst), 0);
    // A renewal refused with time left still asks core to end the pause,
    // within that time and the end's own short bound.
    refuse.store(true, Ordering::SeqCst);
    let started = tokio::time::Instant::now();
    let refused = drain(&config, Duration::from_secs(60), false, PACE)
        .await
        .err()
        .context("a refused renewal drained")?;
    assert!(started.elapsed() < Duration::from_secs(3), "{refused:#}");
    assert!(format!("{refused:#}").contains("core's drain was not shown"));
    assert_eq!(ends.load(Ordering::SeqCst), 1);
    Ok(())
}
