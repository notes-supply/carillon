//! Acceptance of events whose attachments core captures first.
//!
//! A capture runs as core's own task, independent of the request that
//! started it: a caller that times out or disconnects does not stop it, and
//! a retry of the same `(interface, event)` waits for the running capture
//! instead of copying again. At most the configured number of captures run
//! at once; a submission beyond them is refused as unavailable before any
//! work starts. Each capture reserves its bytes against the storage budget
//! until the database decides whether its turn exists.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

use anyhow::anyhow;
use carillon_installation::config::{AttachmentLimits, MIB};
use carillon_protocol::model::{EventReceipt, ExternalEvent, SelectedProvider, StagedAttachments};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, watch};
use tracing::warn;

use crate::{
    Core, CoreError,
    attachments::{self, Cancel, CaptureError},
};

/// How long one capture may copy before it is cancelled. Interfaces wait
/// longer than this for their submission, so they see its outcome.
pub(crate) const CAPTURE_DEADLINE: Duration = Duration::from_mins(5);
/// Bounds on attachment metadata, independent of configured sizes.
const MAX_NAME_BYTES: usize = 1024;
const MAX_CONTENT_TYPE_BYTES: usize = 255;

type Outcome = Option<Result<EventReceipt, Arc<CoreError>>>;
type Key = (String, String);

pub(crate) struct Intake {
    limits: AttachmentLimits,
    pub(crate) workers: Arc<Semaphore>,
    capturing: Mutex<HashMap<Key, watch::Receiver<Outcome>>>,
    /// Woken whenever the last capture in progress settles.
    settled: Notify,
    pub(crate) reserved: Arc<Mutex<u64>>,
    /// Shared by every capture's signal; stopping it stops them all.
    pub(crate) signals: Cancel,
}

/// Check a manifest against core's own limits before anything is read.
pub(crate) fn admit(
    limits: &AttachmentLimits,
    staged: &StagedAttachments,
) -> Result<(), CoreError> {
    let refuse = |message: String| Err(CoreError::InvalidInput(message));
    if !attachments::valid_batch(&staged.batch) {
        return refuse("attachment batch must be 1-64 ASCII letters, digits, '-' or '_'".into());
    }
    if staged.files.is_empty() {
        return refuse("an attachment batch must contain files".into());
    }
    if staged.files.len() > limits.max_files as usize {
        return refuse(format!(
            "at most {} attachments are accepted per message",
            limits.max_files
        ));
    }
    for file in &staged.files {
        if file.name.trim().is_empty() || file.name.len() > MAX_NAME_BYTES {
            return refuse(format!(
                "attachment names must have 1-{MAX_NAME_BYTES} bytes"
            ));
        }
        // Parameters such as `text/plain; charset=utf-8` include spaces.
        if file.content_type.as_ref().is_some_and(|value| {
            value.len() > MAX_CONTENT_TYPE_BYTES
                || !value.bytes().all(|byte| matches!(byte, b' '..=b'~'))
        }) {
            return refuse("attachment media types must be short printable ASCII".into());
        }
        if file.size > limits.max_file_bytes() {
            return refuse(format!(
                "attachments may have at most {} MiB each",
                limits.max_file_mib
            ));
        }
    }
    if total(staged) > limits.max_event_bytes() {
        return refuse(format!(
            "a message's attachments may have at most {} MiB together",
            limits.max_event_mib
        ));
    }
    Ok(())
}

fn total(staged: &StagedAttachments) -> u64 {
    staged.files.iter().map(|file| file.size).sum()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Intake {
    pub(crate) fn new(limits: AttachmentLimits) -> Self {
        Self {
            workers: Arc::new(Semaphore::new(limits.concurrent_captures as usize)),
            limits,
            capturing: Mutex::default(),
            settled: Notify::new(),
            reserved: Arc::default(),
            signals: Cancel::default(),
        }
    }

    pub(crate) fn stop(&self) {
        self.signals.stop_all();
    }

    /// Wait until no capture is in progress: every copy has returned and its
    /// outcome is settled in the database.
    pub(crate) async fn settled(&self) {
        loop {
            let settled = self.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            if lock(&self.capturing).is_empty() {
                return;
            }
            settled.await;
        }
    }

    pub(crate) fn capturing(&self, interface: &str, event_id: &str) -> bool {
        lock(&self.capturing).contains_key(&(interface.to_owned(), event_id.to_owned()))
    }

    /// Join the capture of this event, or start one when a worker is free.
    pub(crate) async fn accept(
        &self,
        core: &Core,
        interface: &SelectedProvider,
        event: &ExternalEvent,
    ) -> Result<EventReceipt, CoreError> {
        let key = (interface.id.clone(), event.event_id.clone());
        let mut outcome = {
            let mut capturing = lock(&self.capturing);
            if let Some(running) = capturing.get(&key) {
                running.clone()
            } else if let Some(receipt) = core
                .database
                .event_receipt(&interface.id, &event.event_id)?
            {
                // A capture of this event committed and left since the
                // caller last looked: its turn is the answer, not a new copy.
                return Ok(receipt);
            } else {
                let staged = event
                    .attachments
                    .as_ref()
                    .ok_or_else(|| anyhow!("event has no attachments to capture"))?;
                if self.signals.stopped() {
                    return Err(CoreError::Unavailable("Carillon is shutting down".into()));
                }
                let permit = self.workers.clone().try_acquire_owned().map_err(|_| {
                    CoreError::Unavailable(
                        "Carillon is already capturing as many attachments as it can; try again shortly"
                            .into(),
                    )
                })?;
                let reservation = self.reserve(core, total(staged))?;
                let (sender, receiver) = watch::channel(None);
                capturing.insert(key.clone(), receiver.clone());
                let run = Run {
                    core: core.clone(),
                    interface: interface.clone(),
                    event: event.clone(),
                    key,
                };
                tokio::spawn(run.capture(permit, reservation, sender));
                receiver
            }
        };
        let settled = outcome
            .wait_for(Option::is_some)
            .await
            .map_err(|_| anyhow!("the attachment capture ended without an outcome"))?;
        match settled.as_ref() {
            Some(Ok(receipt)) => Ok(receipt.clone()),
            Some(Err(refusal)) => Err(refusal.share()),
            None => Err(anyhow!("the attachment capture ended without an outcome").into()),
        }
    }

    /// Reserve bytes against the budget and the free-space floor. Captures
    /// in progress count as written, so concurrent reservations cannot
    /// oversell either.
    fn reserve(&self, core: &Core, bytes: u64) -> Result<Reservation, CoreError> {
        let mut reserved = lock(&self.reserved);
        let kept = core.database.captured_bytes()?;
        let budget = self.limits.storage_bytes();
        if kept + *reserved + bytes > budget {
            return Err(CoreError::InsufficientStorage(format!(
                "Carillon's attachment storage is full: {} of {} MiB are in use or being captured",
                (kept + *reserved).div_ceil(MIB),
                self.limits.storage_mib
            )));
        }
        let free = attachments::available(&core.options.data_directory)
            .map_err(|error| CoreError::Storage(error.into()))?;
        if free < *reserved + bytes + self.limits.free_space_bytes() {
            return Err(CoreError::InsufficientStorage(format!(
                "there is not enough free disk space to keep these attachments and {} MiB spare",
                self.limits.free_space_mib
            )));
        }
        *reserved += bytes;
        Ok(Reservation {
            reserved: self.reserved.clone(),
            bytes,
        })
    }
}

/// Bytes held against the budget; released exactly once, on drop.
struct Reservation {
    reserved: Arc<Mutex<u64>>,
    bytes: u64,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        *lock(&self.reserved) -= self.bytes;
    }
}

/// An event's entry in the in-progress set, removed when dropped.
struct Registered {
    intake: Arc<Intake>,
    key: Key,
}

impl Drop for Registered {
    fn drop(&mut self) {
        let mut capturing = lock(&self.intake.capturing);
        capturing.remove(&self.key);
        if capturing.is_empty() {
            self.intake.settled.notify_waiters();
        }
    }
}

struct Run {
    core: Core,
    interface: SelectedProvider,
    event: ExternalEvent,
    key: Key,
}

impl Run {
    async fn capture(
        self,
        permit: OwnedSemaphorePermit,
        reservation: Reservation,
        sender: watch::Sender<Outcome>,
    ) {
        // Leaves the in-progress set however this task ends, so shutdown
        // never waits for a capture that can no longer settle.
        let _registered = Registered {
            intake: self.core.intake.clone(),
            key: self.key.clone(),
        };
        let outcome = self.capture_and_commit(permit).await;
        // The database has decided: the bytes are now either counted as
        // kept or removed with a losing capture.
        drop(reservation);
        let _ = sender.send(Some(outcome.map_err(Arc::new)));
    }

    async fn capture_and_commit(
        &self,
        permit: OwnedSemaphorePermit,
    ) -> Result<EventReceipt, CoreError> {
        let staged = self
            .event
            .attachments
            .clone()
            .ok_or_else(|| anyhow!("event has no attachments to capture"))?;
        let cancel = self.core.intake.signals.fresh();
        let mut copy = {
            let (data, interface, cancel) = (
                self.core.options.data_directory.clone(),
                self.interface.id.clone(),
                cancel.clone(),
            );
            tokio::task::spawn_blocking(move || {
                // The worker is busy until the copy has really returned,
                // whatever happens to whoever waits for it.
                let _permit = permit;
                attachments::capture(&data, &interface, &staged, &cancel)
            })
        };
        let copied = if let Ok(copied) = tokio::time::timeout(CAPTURE_DEADLINE, &mut copy).await {
            copied
        } else {
            cancel.cancel();
            copy.await
        };
        let capture = match copied.map_err(|error| CoreError::Storage(error.into()))? {
            Ok(capture) => capture,
            Err(CaptureError::Invalid(message)) => return Err(CoreError::InvalidInput(message)),
            Err(CaptureError::Full) => {
                return Err(CoreError::InsufficientStorage(
                    "the disk filled up while these attachments were being captured".into(),
                ));
            }
            Err(CaptureError::Cancelled) => {
                return Err(CoreError::Unavailable(
                    "capturing these attachments did not finish in time".into(),
                ));
            }
            Err(CaptureError::Io(error)) => return Err(CoreError::Storage(error)),
        };
        match self
            .core
            .commit(&self.interface, &self.event, Some(&capture))
        {
            Ok(receipt) => {
                if receipt.duplicate {
                    // Another acceptance of the event won; this copy is unreferenced.
                    self.discard(&capture.id).await;
                }
                Ok(receipt)
            }
            Err(failure) => {
                // Remove the copy only once the database shows no turn uses it.
                match self.core.database.capture_referenced(&capture.id) {
                    Ok(false) => self.discard(&capture.id).await,
                    Ok(true) => {
                        if let Ok(Some(receipt)) = self
                            .core
                            .database
                            .event_receipt(&self.interface.id, &self.event.event_id)
                        {
                            return Ok(receipt);
                        }
                    }
                    Err(error) => {
                        warn!(capture = %capture.id, %error, "capture left for startup reconciliation");
                    }
                }
                Err(failure)
            }
        }
    }

    async fn discard(&self, capture: &str) {
        let (data, capture) = (self.core.options.data_directory.clone(), capture.to_owned());
        let removed =
            tokio::task::spawn_blocking(move || attachments::remove(&data, &capture)).await;
        if !matches!(removed, Ok(Ok(()))) {
            warn!("an unreferenced capture is left for startup reconciliation");
        }
    }
}
