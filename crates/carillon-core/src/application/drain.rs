//! Draining core before maintenance stops the services. The command pauses
//! core's runtime workers and waits until stopping interrupts nothing core
//! runs: no worker left, every answer delivered to its interface while the
//! interface still runs, and every runtime ready to restart. It holds the
//! pause, renewing its lease, until the services have stopped. A drain
//! that does not finish in time, or that core or a runtime cannot show,
//! ends the pause and stops nothing.
//!
//! Forcing stops without waiting. It interrupts: a message a runtime is
//! still running is recorded as interrupted and not replayed, and forcing
//! does not show that native work a runtime started has stopped.

use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use carillon_installation::{config::LoadedConfig, loopback};
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;
use tokio::time::Instant;

use super::{
    http::{Drain, Lease},
    operations::{Action, Services, operator_url},
};

#[derive(Clone, Copy, Debug)]
pub(super) struct Pace {
    /// How long core keeps the pause should this command stop renewing it.
    pub(super) lease: Duration,
    /// How often the pause is renewed while the services stop.
    pub(super) renew: Duration,
    /// How often the drain is checked while waiting for it.
    pub(super) check: Duration,
    /// The longest one request waits for core; a drain report asks the
    /// runtimes, each within five seconds. While waiting for the drain, a
    /// request also waits no longer than what is left of `--timeout`.
    pub(super) request: Duration,
}

impl Pace {
    pub(super) const OPERATOR: Self = Self {
        lease: Duration::from_secs(15),
        renew: Duration::from_secs(3),
        check: Duration::from_millis(500),
        request: Duration::from_secs(15),
    };
}

/// Ending a pause waits only briefly, and while waiting for the drain no
/// longer than what is left of `--timeout`: a pause core does not hear end
/// lapses within its lease anyway.
const END: Duration = Duration::from_secs(2);

/// Core's pause of its runtime workers, held by this command.
pub(super) struct Hold {
    client: Client,
    url: Url,
    token: String,
    pace: Pace,
    /// When the latest request core answered as renewing this pause was
    /// sent. Core keeps it in force for at least the lease from then: both
    /// read the host's one monotonic clock.
    acknowledged: Instant,
}

/// Why core did not report a drain.
enum Refusal {
    /// It holds another pause, or no longer this one.
    Conflict(String),
    Failed(anyhow::Error),
}

impl From<Refusal> for anyhow::Error {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::Conflict(error) => anyhow!(error),
            Refusal::Failed(error) => error,
        }
    }
}

/// Whether the pause held until core stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Held {
    /// Core stopped within the lease of a renewal it acknowledged.
    Throughout,
    /// Core said it had lapsed: it may have claimed work the stop interrupted.
    Lapsed,
    /// No acknowledged renewal covers the time until core stopped: it may
    /// have lapsed while core ran, unseen.
    Unconfirmed,
}

impl Hold {
    async fn begin(config: &LoadedConfig, pace: Pace, within: Duration) -> Result<(Self, Drain)> {
        // A request that may have reached core is never sent again: a
        // pause whose answer was lost lapses on its own.
        let client = loopback::client().retry(reqwest::retry::never()).build()?;
        let token = config.credential("operator")?;
        let lease = Lease {
            lease_ms: u64::try_from(pace.lease.as_millis())?,
        };
        let acknowledged = Instant::now();
        let response = client
            .post(operator_url(config, "/v1/pause")?)
            .bearer_auth(&token)
            .json(&lease)
            .timeout(within)
            .send()
            .await;
        let drain = answer(response)
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| {
                format!(
                    "core at {} did not pause its runtime workers",
                    config.value.core.listen
                )
            })?;
        let url = operator_url(config, &format!("/v1/pause/{}", drain.holder))?;
        Ok((
            Self {
                client,
                url,
                token,
                pace,
                acknowledged,
            },
            drain,
        ))
    }

    async fn renew(&mut self, within: Duration) -> Result<Drain, Refusal> {
        let sent = Instant::now();
        let response = self
            .client
            .put(self.url.clone())
            .bearer_auth(&self.token)
            .timeout(within)
            .send()
            .await;
        let drain = answer(response).await?;
        self.acknowledged = sent;
        Ok(drain)
    }

    /// Ends the pause so core's workers claim again, waiting at most
    /// `within`. With no time left it asks nothing, and the pause lapses
    /// within its lease, as a pause core does not hear end does.
    pub(super) async fn end(&self, within: Duration) {
        if within.is_zero() {
            println!(
                "core's workers resume when its pause lapses, within {} seconds",
                self.pace.lease.as_secs()
            );
            return;
        }
        let ended = self
            .client
            .delete(self.url.clone())
            .bearer_auth(&self.token)
            .timeout(within)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status);
        if let Err(error) = ended {
            println!(
                "core did not confirm that its workers resumed ({error}); its pause lapses within \
                 {} seconds",
                self.pace.lease.as_secs()
            );
        }
    }

    /// Runs `stop`, which ends once core has stopped, while renewing the
    /// pause, so core claims nothing however long the services take to stop
    /// and in whatever order: the service manager stops Discord before core
    /// and the runtimes beside them. A renewal core does not answer shows
    /// nothing; the pause held throughout only if core stopped within the
    /// lease of the last one it acknowledged.
    pub(super) async fn through<T>(&mut self, stop: impl Future<Output = T>) -> (T, Held) {
        let mut stop = std::pin::pin!(stop);
        let mut lapsed = false;
        let stopped = loop {
            let renew = self.pace.renew;
            tokio::select! {
                stopped = &mut stop => break stopped,
                renewed = async {
                    tokio::time::sleep(renew).await;
                    self.renew(self.pace.request).await
                }, if !lapsed => lapsed = matches!(renewed, Err(Refusal::Conflict(_))),
            }
        };
        let held = if lapsed {
            Held::Lapsed
        } else if Instant::now() < self.acknowledged + self.pace.lease {
            Held::Throughout
        } else {
            Held::Unconfirmed
        };
        (stopped, held)
    }
}

async fn answer(response: reqwest::Result<reqwest::Response>) -> Result<Drain, Refusal> {
    #[derive(Deserialize)]
    struct Refused {
        error: String,
    }
    let response = response.map_err(|error| Refusal::Failed(error.into()))?;
    let status = response.status();
    if status.is_success() {
        return response.json().await.map_err(|error| {
            Refusal::Failed(anyhow!(error).context("core's drain report could not be read"))
        });
    }
    let error = response.json::<Refused>().await.map_or_else(
        |_| format!("core answered {status}"),
        |refused| refused.error,
    );
    Err(if status == StatusCode::CONFLICT {
        Refusal::Conflict(error)
    } else {
        Refusal::Failed(anyhow!(error))
    })
}

/// What keeps the services from stopping without interrupting anything;
/// nothing once core has drained.
fn unsettled(drain: &Drain) -> Vec<String> {
    let mut unsettled = Vec::new();
    match drain.workers {
        Some(0) => {
            for runtime in &drain.runtimes {
                if let Some(reason) = &runtime.unready {
                    unsettled.push(format!("{} is not ready to restart: {reason}", runtime.id));
                }
            }
        }
        Some(workers) => unsettled.push(format!(
            "{workers} runtime worker(s) have not ended; a turn one holds settles first"
        )),
        None => unsettled.push("core has not yet arranged its workers for the pause".to_owned()),
    }
    if drain.undelivered > 0 {
        unsettled.push(format!(
            "{} answer(s) not yet delivered by their interface",
            drain.undelivered
        ));
    }
    unsettled
}

/// Pauses core's runtime workers and waits up to `timeout` for core to
/// drain, counting every request to core, and returns the pause to hold
/// while the services stop. Refused, the pause ends and nothing is stopped.
/// Forced, it does not wait, and reports what the stop interrupts.
pub(super) async fn drain(
    config: &LoadedConfig,
    timeout: Duration,
    force: bool,
    pace: Pace,
) -> Result<Option<Hold>> {
    let deadline = Instant::now() + timeout;
    let left = || deadline.saturating_duration_since(Instant::now());
    let within = || left().min(pace.request);
    let refused = |unsettled: &str| {
        anyhow!(
            "core did not drain within {} seconds: {unsettled}. `--force` stops anyway, \
             interrupting it",
            timeout.as_secs()
        )
    };
    let (mut hold, mut drain) = match Hold::begin(config, pace, within()).await {
        Ok(begun) => begun,
        Err(error) if force => {
            println!("Forcing the stop without a drain: {error:#}.");
            return Ok(None);
        }
        Err(error) if left().is_zero() => {
            return Err(error.context(refused(
                "core did not answer in time, and a pause it began meanwhile lapses on its own",
            )));
        }
        Err(error) => return Err(error),
    };
    if force {
        let unsettled = unsettled(&drain);
        if !unsettled.is_empty() {
            println!(
                "Forcing the stop, which interrupts what has not settled: {}. A message a \
                 runtime is still running is recorded as interrupted and not replayed; forcing \
                 does not show that native work a runtime started has stopped.",
                unsettled.join("; ")
            );
        }
        return Ok(Some(hold));
    }
    let mut shown = String::new();
    loop {
        let unsettled = unsettled(&drain).join("; ");
        if unsettled.is_empty() {
            return Ok(Some(hold));
        }
        if left().is_zero() {
            hold.end(Duration::ZERO).await;
            return Err(refused(&unsettled));
        }
        if unsettled != shown {
            println!("Waiting for core to drain: {unsettled}.");
            shown = unsettled;
        }
        tokio::time::sleep(pace.check.min(left())).await;
        drain = match hold.renew(within()).await {
            Ok(drain) => drain,
            Err(refusal) => {
                hold.end(left().min(END)).await;
                let refusal = anyhow::Error::from(refusal);
                // A report cut off by the deadline leaves the last one's account.
                return Err(if left().is_zero() {
                    refusal.context(refused(&shown))
                } else {
                    refusal.context("core's drain was not shown")
                });
            }
        };
    }
}

/// Stops the services, holding core's pause until the service manager
/// reports them stopped. If they did not stop, the pause ends so core
/// claims again.
pub(super) async fn stop(services: &Services<'_>, hold: Option<Hold>) -> Result<()> {
    let Some(mut hold) = hold else {
        return services.control(Action::Stop).await;
    };
    let (stopped, kept) = hold.through(services.control(Action::Stop)).await;
    if stopped.is_err() {
        hold.end(END).await;
        return stopped;
    }
    match kept {
        Held::Throughout => println!("Core stopped with its runtime workers paused."),
        Held::Lapsed => println!(
            "Core's pause lapsed before core stopped: its workers may have claimed a turn the \
             stop interrupted; `carillon recent` shows what ran."
        ),
        Held::Unconfirmed => println!(
            "Core did not confirm its pause until it stopped, so its workers may have claimed a \
             turn the stop interrupted; `carillon recent` shows what ran."
        ),
    }
    stopped
}
