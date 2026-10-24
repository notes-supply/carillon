//! Discord calls bounded as a whole, and what each one proves.

use std::{
    future::pending,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::watch,
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;
use twilight_http::{
    error::ErrorType,
    response::{DeserializeBodyError, Response, ResponseFuture},
};

use super::{EXCHANGE_LIMIT, Known};

/// What a bounded Discord call established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Call<T> {
    Done(T),
    /// Discord certainly did not receive it.
    NotSent,
    /// Discord answered with this client error status: it had no effect.
    Refused(u16),
    /// It may or may not have taken effect.
    Unknown,
}

/// While a request waits for the rate limiter, the provider must not be
/// stopping and, for a request about how its job stands, the job must
/// still want it.
pub(super) struct Wanted {
    pub(super) job: Option<JobNeed>,
    pub(super) stopping: CancellationToken,
}

/// What of its job a request needs, watched on the job's view.
#[derive(Clone)]
pub(super) struct JobNeed {
    view: watch::Receiver<Known>,
    holds: fn(&Known) -> bool,
}

pub(super) fn job_wanted(
    view: &watch::Receiver<Known>,
    stopping: &CancellationToken,
    holds: fn(&Known) -> bool,
) -> Wanted {
    Wanted {
        job: Some(JobNeed {
            view: view.clone(),
            holds,
        }),
        stopping: stopping.clone(),
    }
}

pub(super) async fn done<T>(_: Response<T>) -> Result<(), DeserializeBodyError> {
    Ok(())
}

/// Sends a Discord request with a bound on the whole call: waiting for the
/// rate limiter (including Twilight's internal retries after 429), the
/// exchange and decoding. Twilight's pre-flight check, evaluated each time
/// the limiter grants a permit, stops a request once `wait` has passed or it
/// is no longer wanted; a request still waiting when it stops being wanted
/// is dropped at once. Once a request may have left, the call waits for its
/// answer rather than racing a later request to the same message.
pub(super) async fn bounded<T: Unpin, U>(
    mut request: ResponseFuture<T>,
    wait: Duration,
    wanted: Option<Wanted>,
    decode: impl AsyncFnOnce(Response<T>) -> Result<U, DeserializeBodyError>,
) -> Call<U> {
    let deadline = Instant::now() + wait;
    let sent = Arc::new(AtomicBool::new(false));
    let marked = sent.clone();
    let check = wanted
        .as_ref()
        .map(|wanted| (wanted.job.clone(), wanted.stopping.clone()));
    let preflight = request.set_pre_flight(move || {
        let proceed = Instant::now() < deadline
            && check.as_ref().is_none_or(|(job, stopping)| {
                !stopping.is_cancelled()
                    && job
                        .as_ref()
                        .is_none_or(|need| (need.holds)(&need.view.borrow()))
            });
        // Only a passed check sends; a refused one proves nothing was sent.
        if proceed {
            marked.store(true, Ordering::SeqCst);
        }
        proceed
    });
    if !preflight {
        // Without a rate limiter the request leaves when first polled.
        sent.store(true, Ordering::SeqCst);
    }
    let exchange = async {
        match request.await {
            Ok(response) => match decode(response).await {
                Ok(value) => Call::Done(value),
                // Discord performed it, but its answer is unusable.
                Err(_) => Call::Unknown,
            },
            Err(error) => match error.kind() {
                ErrorType::RequestCanceled if !sent.load(Ordering::SeqCst) => Call::NotSent,
                ErrorType::Response { status, .. }
                    if status.is_client_error() && status.get() != 429 =>
                {
                    Call::Refused(status.get())
                }
                _ => Call::Unknown,
            },
        }
    };
    let limit = deadline + EXCHANGE_LIMIT;
    let unsent = || {
        if sent.load(Ordering::SeqCst) {
            Call::Unknown
        } else {
            Call::NotSent
        }
    };
    let mut exchange = std::pin::pin!(timeout_at(limit, exchange));
    tokio::select! {
        biased;
        outcome = &mut exchange => outcome.unwrap_or_else(|_| unsent()),
        () = unwanted(wanted) => {
            if sent.load(Ordering::SeqCst) {
                exchange.await.unwrap_or(Call::Unknown)
            } else {
                Call::NotSent
            }
        }
    }
}

async fn unwanted(wanted: Option<Wanted>) {
    let Some(Wanted { job, stopping }) = wanted else {
        return pending().await;
    };
    let job_gone = async {
        match job {
            Some(JobNeed { mut view, holds }) => {
                let _ = view.wait_for(|known| !holds(known)).await;
            }
            // Its job's end, or the closing of its view, does not end it.
            None => pending().await,
        }
    };
    tokio::select! {
        () = job_gone => {}
        () = stopping.cancelled() => {}
    }
}
