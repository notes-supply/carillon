//! Work cards and typing while jobs run.
//!
//! A job is the work one message began: the message's turn and the
//! observations core queues to report what it delegated. Core owns every
//! fact of a job: its turns, their final reports and how each answer was
//! sent. The monitor reads them every few seconds, every job at work page by
//! page and, by name, every job it still follows that is no longer at work,
//! and at once when the delivery loop has had an answer's outcome recorded.
//! The delivery loop names a job when it takes up one of its answers, before
//! the job can settle, so a job that ends between two reads is still
//! followed. The monitor never infers how a job ended from a job no longer
//! being at work. Each job's card and typing are separate tasks watching the latest
//! reading, so one card's slow or rate-limited Discord call never delays
//! another job, and nothing here is awaited by delivery. When the provider
//! stops it reads, types and makes cards no more, but takes the jobs the
//! last deliveries name, reads them once more and gives the cards they
//! settle a bounded moment to show their summaries before it returns.
//!
//! The monitor alone makes a job's card: one embed, edited as the job goes
//! on and kept as its summary when the job earned one, or removed when it
//! did not. A job that ended before its card appeared and earned a summary
//! gets a card showing it. Answers are plain text. What a card shows, and
//! whether a settled job keeps it, is the Discord module's to decide; a
//! module that gives no usable card leaves the card as it was.
//!
//! Every Discord call is bounded, and its outcome is classified by what it
//! proves: a job gets at most one card, and a card whose creation is unknown
//! or which someone deleted is never followed by another before the job
//! settles. The provider records only what Discord alone knows: a card's
//! message, or that one nobody can name may exist. A restart goes on with
//! the recorded cards from what core recorded of their jobs.
use crate::{client::CoreClient, config::Settings, module::Module};
use anyhow::{Context as _, Result};
use carillon_discord_module::contract::{CardRequest, Seen};
use carillon_installation::filesystem::{PublishError, publish};
use carillon_protocol::model::{JOB_READ_LIMIT, Job, TurnState};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet, hash_map::Entry},
    fs,
    future::pending,
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
    time::{Instant, MissedTickBehavior, interval, sleep, timeout, timeout_at},
};
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};
use twilight_http::{
    Client as DiscordHttp,
    error::ErrorType,
    response::{DeserializeBodyError, Response, ResponseFuture},
};
use twilight_model::{
    channel::message::{AllowedMentions, Embed},
    id::{
        Id,
        marker::{ChannelMarker, MessageMarker},
    },
};

/// How often core is read.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// A job whose turn finishes sooner after acceptance gets no card of its own.
const APPEAR_AFTER: Duration = Duration::from_secs(2);
/// The least time between two edits of one card.
const EDIT_INTERVAL: Duration = Duration::from_secs(3);
/// Discord shows typing for ten seconds; refresh before it lapses.
const TYPING_REFRESH: Duration = Duration::from_secs(8);
/// How long a request may wait for Discord's rate limiter before it is no
/// longer worth sending. Progress is superseded by the next read; a card's
/// creation and its summary are worth waiting longer for.
const PROGRESS_WAIT: Duration = Duration::from_secs(3);
const CARD_WAIT: Duration = Duration::from_secs(10);
/// Once sent, Twilight's own 10-second HTTP timeout and response decoding.
const EXCHANGE_LIMIT: Duration = Duration::from_secs(12);
/// Attempts to finish a card before leaving it to the next start.
const FINAL_ATTEMPTS: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// How long a stopping provider gives its last read and the cards it
/// settles to show their summaries: one final edit's whole bound. A card
/// still unfinished then is left to the next start.
const FINAL_DRAIN: Duration = CARD_WAIT.saturating_add(EXCHANGE_LIMIT);

/// What the delivery loop tells the monitor of a job's answer or notice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Delivering {
    /// It took up an answer of the job its origin turn names: the job is
    /// followed from now.
    Claimed(String),
    /// Core recorded how sending an answer went: the next read shows it.
    Recorded,
}

/// The monitor's latest knowledge of one job.
#[derive(Clone, Debug, Default)]
enum Known {
    /// Followed, but no read has shown it yet.
    #[default]
    Unread,
    /// What core recorded of it, as the latest read showed.
    Read(Job),
    /// Core holds no such job of this provider's.
    Unknown,
}

impl Known {
    fn job(&self) -> Option<&Job> {
        match self {
            Self::Read(job) => Some(job),
            Self::Unread | Self::Unknown => None,
        }
    }

    /// Nothing more will happen to the job.
    fn is_over(&self) -> bool {
        match self {
            Self::Read(job) => job.settled().is_some(),
            Self::Unknown => true,
            Self::Unread => false,
        }
    }

    /// The channel a running turn of the job answers in.
    fn running_in(&self) -> Option<Id<ChannelMarker>> {
        let job = self.job()?;
        job.at_work()
            .filter(|turn| turn.state == TurnState::Running)
            .and_then(|_| job.target_id.parse().ok())
    }
}

/// The Discord message showing a job's work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Card {
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
}

/// What a later start must know of a job's card, which only Discord knows:
/// the card, or that the job has one nobody can name or follow, whose
/// creation was never confirmed or which someone deleted, so it gets no
/// other. A pending creation is known not to have reached Discord and may
/// be retried; core supplies the summary, not this record.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Recorded {
    channel_id: Id<ChannelMarker>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message_id: Option<Id<MessageMarker>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pending_create: bool,
}

impl Recorded {
    fn holding(self) -> Holding {
        match self.message_id {
            Some(message_id) => Holding::Card {
                card: Card {
                    channel_id: self.channel_id,
                    message_id,
                },
                shown: None,
                editable: true,
                uncertain_edit: false,
            },
            None if self.pending_create => Holding::Nothing,
            None => Holding::Held,
        }
    }
}

/// Cards of jobs whose summary is not final yet, by job. Creation is
/// suppressed durably before it can reach Discord; its message identity is
/// recorded once Discord confirms it. A crash in between leaves a card no
/// later start can name, but not another creation. An unknown creation, or a card
/// someone deleted, is recorded without a message until the job settles, so
/// no start makes the job another card. An unsent summary creation is also
/// recorded for retry. A record goes once its card shows the job's summary,
/// or is gone.
pub(crate) struct CardRecords {
    path: PathBuf,
    cards: Mutex<BTreeMap<String, Recorded>>,
}

impl CardRecords {
    /// Opens the records, or returns `None` when they cannot be read: without
    /// them nobody knows which cards exist, so cards stay off for this start
    /// rather than risk second ones. The file is left as it is for the
    /// operator. An absent file is simply no records.
    pub(crate) fn open(path: PathBuf) -> Option<Self> {
        let cards = match fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        };
        match cards {
            Ok(cards) => Some(Self {
                path,
                cards: Mutex::new(cards),
            }),
            Err(failure) => {
                error!(
                    error = format!("{failure:#}"),
                    "activity cards are off until their records can be read"
                );
                None
            }
        }
    }

    fn recorded(&self) -> BTreeMap<String, Recorded> {
        lock(&self.cards).clone()
    }

    fn insert(&self, job: &str, card: Recorded) -> bool {
        // An identical in-memory record need not be durable: the previous
        // publication may have failed. Every insert confirms it anew.
        self.change(|cards| {
            cards.insert(job.to_owned(), card);
            true
        })
    }

    fn remove(&self, job: &str) {
        self.change(|cards| cards.remove(job).is_some());
    }

    /// Publishes the records whole when `change` changed them. A record that
    /// was not removed on disk is finished again by the next start; one that
    /// was not added leaves a card no later start knows of.
    fn change(&self, change: impl FnOnce(&mut BTreeMap<String, Recorded>) -> bool) -> bool {
        let mut cards = lock(&self.cards);
        if !change(&mut cards) {
            return true;
        }
        let saved = serde_json::to_vec_pretty(&*cards)
            .map_err(|error| PublishError::NotPublished(error.into()))
            .and_then(|bytes| publish(&self.path, &bytes, 0o600));
        if let Err(error) = saved {
            warn!(path = %self.path.display(), %error, "activity card records were not saved");
            return false;
        }
        true
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What every job task shares.
#[derive(Clone)]
struct Cards {
    http: Arc<DiscordHttp>,
    module: Arc<Module>,
    /// The settings serving: each card is drawn with the markers they give.
    settings: Settings,
    records: Arc<CardRecords>,
    /// The provider is stopping: no new card is made.
    stopping: CancellationToken,
}

/// Follows this provider's jobs until `delivering` closes. Cards recorded
/// before this start are taken up again. Without readable
/// `records`, or with activity cards turned off when the monitor starts,
/// there are no cards, only typing: recorded cards and their records are
/// left as they are. Each job shows typing when the settings serving as it
/// is first followed say so. Once
/// `stopping`, nothing more is read, typed or made a card; when `delivering`
/// closes, core is read once more and the cards of jobs that settled get up
/// to [`FINAL_DRAIN`] to show their summaries, and every other card stays
/// recorded for the next start.
pub(crate) async fn activity_monitor(
    core: CoreClient,
    http: Arc<DiscordHttp>,
    module: Arc<Module>,
    settings: Settings,
    records: Option<CardRecords>,
    mut delivering: mpsc::UnboundedReceiver<Delivering>,
    stopping: CancellationToken,
) {
    let typist = http.clone();
    let cards = records
        .filter(|_| settings.current().activity)
        .map(|records| Cards {
            http,
            module,
            settings: settings.clone(),
            records: Arc::new(records),
            stopping: stopping.clone(),
        });
    let mut tasks = JoinSet::new();
    let mut jobs = BTreeMap::new();
    let follow = |tasks: &mut JoinSet<()>, job_id: &str, known: Known, old: Option<Recorded>| {
        let (view, watched) = watch::channel(known);
        if settings.current().typing {
            tasks.spawn(keep_typing(
                typist.clone(),
                watched.clone(),
                stopping.clone(),
            ));
        }
        if let Some(cards) = &cards {
            tasks.spawn(show_card(cards.clone(), job_id.to_owned(), watched, old));
        }
        view
    };
    for (job_id, recorded) in cards
        .as_ref()
        .map(|cards| cards.records.recorded())
        .unwrap_or_default()
    {
        let view = follow(&mut tasks, &job_id, Known::Unread, Some(recorded));
        jobs.insert(job_id, view);
    }
    let mut reading = None;
    let mut ticks = interval(POLL_INTERVAL);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut reachable = true;
    loop {
        tokio::select! {
            biased;
            news = delivering.recv() => match news {
                None => break,
                Some(Delivering::Claimed(job_id)) => {
                    if cards.is_some() && !stopping.is_cancelled() && !jobs.contains_key(&job_id) {
                        let view = follow(&mut tasks, &job_id, Known::Unread, None);
                        jobs.insert(job_id, view);
                    }
                }
                Some(Delivering::Recorded) => ticks.reset_immediately(),
            },
            // Core is read each interval, one read at a time, until the
            // provider stops.
            _ = ticks.tick(), if reading.is_none() && !stopping.is_cancelled() => {
                let named = cards.is_some().then(|| jobs.keys().cloned().collect());
                reading = Some(Box::pin(read(core.clone(), named.unwrap_or_default())));
            }
            // A read taken before the provider stopped is not shown after.
            () = stopping.cancelled(), if reading.is_some() => reading = None,
            Some(read) = async { Some(reading.as_mut()?.await) } => {
                reading = None;
                match read {
                    Ok(read) => {
                        reachable = true;
                        for job in take_read(read, cards.is_some(), &mut jobs) {
                            let job_id = job.origin.turn_id.clone();
                            let view = follow(&mut tasks, &job_id, Known::Read(job), None);
                            jobs.insert(job_id, view);
                        }
                    }
                    // Unknown is not ended: every card keeps what it shows.
                    Err(failure) => {
                        if reachable {
                            warn!(error = %failure.message, "cannot read jobs from core");
                        }
                        reachable = false;
                    }
                }
                while tasks.try_join_next().is_some() {}
            }
        }
    }
    drop(reading);
    let last = async {
        // The jobs the last deliveries settled show their summaries.
        if cards.is_some() && !jobs.is_empty() {
            let named = jobs.keys().cloned().collect::<Vec<_>>();
            match read_named(&core, &named, &HashSet::new()).await {
                Ok(read) => {
                    take_read(read, true, &mut jobs);
                }
                Err(failure) => {
                    warn!(error = %failure.message, "cannot read jobs from core at stop");
                }
            }
        }
        // Jobs still open end their tasks, which leave their cards recorded.
        drop(jobs);
        while tasks.join_next().await.is_some() {}
    };
    if timeout(FINAL_DRAIN, last).await.is_err() {
        warn!("activity cards still finishing at stop are left to the next start");
    }
}

/// What one read of core showed: every job at work, and every job `named`
/// that is not, as core recorded them, and the named ones core holds no such
/// job of.
struct Read {
    jobs: Vec<Job>,
    unknown: Vec<String>,
}

/// Reads every job at work, a page at a time, then the jobs `named` that
/// none of the pages listed.
async fn read(core: CoreClient, named: Vec<String>) -> Result<Read, crate::InterfaceError> {
    let mut jobs = Vec::new();
    let mut positions = HashMap::new();
    let mut after = None;
    loop {
        let page = core.jobs_at_work(after.as_deref()).await?;
        // A continuation accepted between pages can list the same origin
        // twice. Its latest read wins, with still only one card owner.
        for job in page.jobs {
            match positions.entry(job.origin.turn_id.clone()) {
                Entry::Occupied(position) => jobs[*position.get()] = job,
                Entry::Vacant(position) => {
                    position.insert(jobs.len());
                    jobs.push(job);
                }
            }
        }
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    let listed = positions.into_keys().collect::<HashSet<_>>();
    let mut read = read_named(&core, &named, &listed).await?;
    read.jobs.extend(jobs);
    Ok(read)
}

/// Reads the jobs `named` that are not `listed` already, a bounded number
/// at a time.
async fn read_named(
    core: &CoreClient,
    named: &[String],
    listed: &HashSet<String>,
) -> Result<Read, crate::InterfaceError> {
    let unlisted = named
        .iter()
        .filter(|job| !listed.contains(*job))
        .cloned()
        .collect::<Vec<_>>();
    let mut read = Read {
        jobs: Vec::new(),
        unknown: Vec::new(),
    };
    for asked in unlisted.chunks(JOB_READ_LIMIT) {
        let answered = core.named_jobs(asked).await?;
        read.unknown.extend(
            asked
                .iter()
                .filter(|job| !answered.iter().any(|known| &known.origin.turn_id == *job))
                .cloned(),
        );
        read.jobs.extend(answered);
    }
    Ok(read)
}

/// Shows a read on the jobs it names and returns the jobs at work not
/// followed yet. With cards, a followed job the read does not mention was
/// followed after it began; without them, one is no longer at work and is
/// let go.
fn take_read(
    read: Read,
    cards: bool,
    jobs: &mut BTreeMap<String, watch::Sender<Known>>,
) -> Vec<Job> {
    let mut mentioned = HashSet::new();
    let mut new = Vec::new();
    for job in read.jobs {
        let job_id = job.origin.turn_id.clone();
        if let Some(view) = jobs.get(&job_id) {
            view.send_replace(Known::Read(job));
        } else if job.at_work().is_some() {
            new.push(job);
        }
        mentioned.insert(job_id);
    }
    for job_id in read.unknown {
        if let Some(view) = jobs.get(&job_id) {
            view.send_replace(Known::Unknown);
        }
        mentioned.insert(job_id);
    }
    // A job stays followed while any of its tasks runs, so it never gets a
    // second card.
    jobs.retain(|job_id, view| !view.is_closed() && (cards || mentioned.contains(job_id)));
    new
}

/// Types in a job's channel while one of its turns runs, until it ends or
/// the provider stops.
async fn keep_typing(
    http: Arc<DiscordHttp>,
    mut view: watch::Receiver<Known>,
    stopping: CancellationToken,
) {
    let mut typed: Option<Instant> = None;
    while !stopping.is_cancelled() {
        let channel = {
            let known = view.borrow_and_update();
            if known.is_over() {
                return;
            }
            known.running_in()
        };
        if let Some(channel) = channel
            && typed.is_none_or(|at| at.elapsed() >= TYPING_REFRESH)
        {
            typed = Some(Instant::now());
            let request = http.create_typing_trigger(channel).into_future();
            let wanted = job_wanted(&view, &stopping, |known| known.running_in().is_some());
            if let Call::Refused(status) = bounded(request, PROGRESS_WAIT, Some(wanted), done).await
            {
                warn!(status, "Discord refused a typing indicator");
            }
        }
        let waited = timeout(TYPING_REFRESH, view.changed());
        if let Some(Ok(Err(_))) = stopping.run_until_cancelled(waited).await {
            return;
        }
    }
}

/// The job's card as its task knows it.
enum Holding {
    /// None yet: one is made once the job has worked past the appearance
    /// delay, or with its summary if it ends sooner.
    Nothing,
    /// A card to edit, with what it shows when this task showed it.
    Card {
        card: Card,
        shown: Option<Box<Embed>>,
        editable: bool,
        /// An earlier write may still land after the final edit.
        uncertain_edit: bool,
    },
    /// One nobody can name or follow, or one Discord refused to make: the
    /// job gets no other.
    Held,
}

/// One job's card, from the card recorded before this start or the one
/// made once the job is worth it, followed until the job's summary is final.
/// The job stays followed until then, so a card nobody can name keeps it
/// from another until it settles.
async fn show_card(
    cards: Cards,
    job_id: String,
    mut view: watch::Receiver<Known>,
    old: Option<Recorded>,
) {
    let mut holding = old.map_or(Holding::Nothing, Recorded::holding);
    let mut seen: Option<Seen> = None;
    let mut next_edit = Instant::now();
    let mut failed = false;
    loop {
        let known = view.borrow_and_update().clone();
        let job = match known {
            Known::Unread => {
                if view.changed().await.is_err() {
                    return;
                }
                continue;
            }
            // Core has nothing of the job: its card says nothing true.
            Known::Unknown => {
                cards.finish(&job_id, holding, None, None).await;
                return;
            }
            Known::Read(job) => job,
        };
        if let Some(turn) = job.at_work()
            && let Some(snapshot) = &turn.activity
        {
            seen = Some(Seen {
                turn_id: turn.turn_id.clone(),
                activity: snapshot.clone(),
            });
        }
        if job.settled().is_some() {
            cards.settle(&job_id, holding, job, seen, &mut failed).await;
            return;
        }
        let channel = job.target_id.parse().ok();
        match &mut holding {
            Holding::Nothing => {
                // A job that ended but whose answer is not yet sent waits
                // for it; a stopping provider makes no new card.
                if let Some(turn) = job.at_work()
                    && !cards.stopping.is_cancelled()
                {
                    let appear = Instant::now() + until_after(turn.accepted_at, APPEAR_AFTER);
                    if Instant::now() < appear {
                        match timeout_at(appear, view.changed()).await {
                            Ok(Err(_)) => return,
                            Ok(Ok(())) | Err(_) => continue,
                        }
                    }
                    let rendered = cards.render(&job_id, job, seen.clone(), &mut failed);
                    if let Ok(Some(embed)) = rendered.await {
                        holding = cards.create_progress(&job_id, channel, &embed).await;
                        next_edit = Instant::now() + EDIT_INTERVAL;
                    }
                }
            }
            Holding::Card {
                card,
                shown,
                editable: true,
                ..
            } => {
                let rendered = cards.render(&job_id, job, seen.clone(), &mut failed);
                if let Ok(Some(embed)) = rendered.await
                    && shown.as_deref() != Some(&embed)
                {
                    // Edits wait out the interval; the job's end does not.
                    if Instant::now() < next_edit {
                        match timeout_at(next_edit, view.wait_for(Known::is_over)).await {
                            Ok(Err(_)) => return,
                            Ok(Ok(_)) | Err(_) => continue,
                        }
                    }
                    let wanted = job_wanted(&view, &cards.stopping, |known| !known.is_over());
                    let edit = cards.edit(*card, &embed, Some(wanted)).await;
                    holding = cards.edited(&job_id, holding, edit, embed);
                    next_edit = Instant::now() + EDIT_INTERVAL;
                }
            }
            Holding::Card { .. } | Holding::Held => {}
        }
        if view.changed().await.is_err() {
            // The monitor stopped: the record lets the next start go on.
            return;
        }
    }
}

/// Time from now until `after` past a Unix-seconds timestamp.
fn until_after(accepted_at: i64, after: Duration) -> Duration {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let accepted = Duration::from_secs(u64::try_from(accepted_at).unwrap_or_default());
    (accepted + after).saturating_sub(now).min(after)
}

impl Cards {
    /// The job's card as the module serving now renders it: the embed to
    /// show, or `None` when a settled job's card goes. A failure keeps the
    /// card as it was, and is logged unless `failed` says it already was.
    async fn render(
        &self,
        job_id: &str,
        job: Job,
        seen: Option<Seen>,
        failed: &mut bool,
    ) -> Result<Option<Embed>, ()> {
        let request = CardRequest {
            job,
            seen,
            markers: self.settings.current().activity_markers.clone(),
        };
        self.module.render_card(&request).await.map_err(|error| {
            if !std::mem::replace(failed, true) {
                warn!(
                    job_id,
                    error = format!("{error:#}"),
                    "the Discord module gave no usable card"
                );
            }
        })
    }

    /// Finishes a settled job's card with the summary the module draws, or
    /// without one when it draws none. A card the module could not draw keeps
    /// its record, so the next start finishes it; one nobody can name needs
    /// no drawing.
    async fn settle(
        &self,
        job_id: &str,
        holding: Holding,
        job: Job,
        seen: Option<Seen>,
        failed: &mut bool,
    ) {
        let channel = job.target_id.parse().ok();
        let summary = match holding {
            Holding::Held => None,
            Holding::Nothing | Holding::Card { .. } => {
                let Ok(summary) = self.render(job_id, job, seen, failed).await else {
                    return;
                };
                summary
            }
        };
        self.finish(job_id, holding, summary, channel).await;
    }

    /// What a card is once its progress edit to `embed` ended as `edit`.
    fn edited(&self, job_id: &str, holding: Holding, edit: Call<()>, embed: Embed) -> Holding {
        let Holding::Card {
            card,
            shown,
            uncertain_edit,
            ..
        } = holding
        else {
            return holding;
        };
        let (shown, editable, uncertain_edit) = match edit {
            Call::Done(()) => (Some(Box::new(embed)), true, uncertain_edit),
            // The next read brings the latest again.
            Call::NotSent => (shown, true, uncertain_edit),
            Call::Unknown => {
                warn!(job_id, "activity card edit outcome is unknown");
                (shown, false, true)
            }
            Call::Refused(404) => {
                // Deleted by someone: the job gets no other card, even after
                // a restart, before it settles.
                self.records.insert(
                    job_id,
                    Recorded {
                        channel_id: card.channel_id,
                        message_id: None,
                        pending_create: false,
                    },
                );
                warn!(job_id, "activity card was deleted in Discord");
                return Holding::Held;
            }
            Call::Refused(status) => {
                warn!(job_id, status, "activity card can no longer be edited");
                (shown, false, uncertain_edit)
            }
        };
        Holding::Card {
            card,
            shown,
            editable,
            uncertain_edit,
        }
    }

    /// Makes the card of a job at work in `channel`, showing `embed`, and
    /// records it.
    async fn create_progress(
        &self,
        job_id: &str,
        channel: Option<Id<ChannelMarker>>,
        embed: &Embed,
    ) -> Holding {
        let Some(channel_id) = channel else {
            warn!(job_id, "activity card target is not a Discord channel");
            return Holding::Held;
        };
        match self.create(job_id, channel_id, embed).await {
            Call::Done(message_id) => {
                let recorded = Recorded {
                    channel_id,
                    message_id: Some(message_id),
                    pending_create: false,
                };
                self.records.insert(job_id, recorded);
                Holding::Card {
                    card: Card {
                        channel_id,
                        message_id,
                    },
                    shown: Some(Box::new(embed.clone())),
                    editable: true,
                    uncertain_edit: false,
                }
            }
            // Tried again with the job's next change.
            Call::NotSent => {
                self.records.insert(
                    job_id,
                    Recorded {
                        channel_id,
                        message_id: None,
                        pending_create: true,
                    },
                );
                Holding::Nothing
            }
            Call::Refused(status) => {
                warn!(job_id, status, "Discord refused this job's activity card");
                Holding::Held
            }
            Call::Unknown => {
                // Discord may hold a card this task cannot name: the job
                // gets no other, even after a restart.
                warn!(
                    job_id,
                    "an activity card of unknown identity may be in Discord; the job gets no other"
                );
                Holding::Held
            }
        }
    }

    /// Leaves a settled job's card showing its `summary`, or removes it when
    /// there is none to show. A job that ended before it had a card gets
    /// a card with the summary it earned in
    /// `channel`, unless the provider is stopping. The record goes once
    /// nothing is left to finish; a card Discord did not answer for stays
    /// recorded for the next start.
    async fn finish(
        &self,
        job_id: &str,
        holding: Holding,
        summary: Option<Embed>,
        channel: Option<Id<ChannelMarker>>,
    ) {
        let (card, uncertain_edit) = match holding {
            Holding::Card {
                card,
                uncertain_edit,
                ..
            } => (card, uncertain_edit),
            Holding::Nothing => {
                if let (Some(summary), Some(channel)) = (summary, channel) {
                    self.create_summary(job_id, channel, &summary).await;
                } else {
                    self.records.remove(job_id);
                }
                return;
            }
            Holding::Held => {
                self.records.remove(job_id);
                return;
            }
        };
        for attempt in 1..=FINAL_ATTEMPTS {
            let call = match &summary {
                Some(embed) => self.edit(card, embed, None).await,
                None => self.delete(card).await,
            };
            match call {
                Call::Done(()) => {
                    if uncertain_edit && summary.is_some() {
                        warn!(
                            job_id,
                            "an earlier card edit may still arrive; its identity is retained for the next start"
                        );
                    } else {
                        self.records.remove(job_id);
                    }
                    return;
                }
                Call::Refused(404) => {
                    self.records.remove(job_id);
                    return;
                }
                Call::Refused(status) => {
                    warn!(
                        job_id,
                        status,
                        "Discord refused to finish an activity card; its record is retained"
                    );
                    return;
                }
                Call::NotSent | Call::Unknown if attempt < FINAL_ATTEMPTS => {
                    sleep(RETRY_DELAY).await;
                }
                Call::NotSent | Call::Unknown => {}
            }
        }
        warn!(
            job_id,
            "activity card was not finished; the next start goes on with it"
        );
    }

    /// Retries only creations proven not sent. An in-flight or unknown
    /// creation suppresses another card; a known unsent one is left for
    /// the next start to finish from core's facts.
    async fn create_summary(&self, job_id: &str, channel_id: Id<ChannelMarker>, summary: &Embed) {
        let recorded = Recorded {
            channel_id,
            message_id: None,
            pending_create: true,
        };
        self.records.insert(job_id, recorded);
        for attempt in 1..=FINAL_ATTEMPTS {
            if self.stopping.is_cancelled() {
                return;
            }
            match self.create(job_id, channel_id, summary).await {
                Call::Done(_) => {
                    self.records.remove(job_id);
                    return;
                }
                Call::NotSent => {
                    self.records.insert(job_id, recorded);
                    if attempt < FINAL_ATTEMPTS {
                        sleep(RETRY_DELAY).await;
                    }
                }
                Call::Refused(status) => {
                    self.records.insert(job_id, recorded);
                    warn!(
                        job_id,
                        status, "Discord refused this job's summary card; its retry is retained"
                    );
                    return;
                }
                Call::Unknown => {
                    warn!(
                        job_id,
                        "Discord may hold this job's summary card unconfirmed"
                    );
                    return;
                }
            }
        }
        warn!(
            job_id,
            "summary card was not sent; the next start retries it"
        );
    }

    /// Makes a card. It is wanted until the provider stops, however its job
    /// ends meanwhile, so the card then shows the job's end.
    async fn create(
        &self,
        job_id: &str,
        channel: Id<ChannelMarker>,
        embed: &Embed,
    ) -> Call<Id<MessageMarker>> {
        if !self.records.insert(
            job_id,
            Recorded {
                channel_id: channel,
                message_id: None,
                pending_create: false,
            },
        ) {
            warn!(
                job_id,
                "card creation deferred until its suppression can be saved"
            );
            return Call::NotSent;
        }
        let silent = AllowedMentions::default();
        let embeds = [embed.clone()];
        let request = self
            .http
            .create_message(channel)
            .allowed_mentions(Some(&silent))
            .embeds(&embeds)
            .into_future();
        bounded(
            request,
            CARD_WAIT,
            Some(Wanted {
                job: None,
                stopping: self.stopping.clone(),
            }),
            async |response| Ok(response.model().await?.id),
        )
        .await
    }

    async fn edit(&self, card: Card, embed: &Embed, wanted: Option<Wanted>) -> Call<()> {
        let silent = AllowedMentions::default();
        let embeds = [embed.clone()];
        let request = self
            .http
            .update_message(card.channel_id, card.message_id)
            .allowed_mentions(Some(&silent))
            .embeds(Some(&embeds))
            .into_future();
        let wait = if wanted.is_some() {
            PROGRESS_WAIT
        } else {
            CARD_WAIT
        };
        bounded(request, wait, wanted, done).await
    }

    async fn delete(&self, card: Card) -> Call<()> {
        let request = self
            .http
            .delete_message(card.channel_id, card.message_id)
            .into_future();
        bounded(request, CARD_WAIT, None, done).await
    }
}

/// What a bounded Discord call established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call<T> {
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
struct Wanted {
    job: Option<JobNeed>,
    stopping: CancellationToken,
}

/// What of its job a request needs, watched on the job's view.
#[derive(Clone)]
struct JobNeed {
    view: watch::Receiver<Known>,
    holds: fn(&Known) -> bool,
}

fn job_wanted(
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

async fn done<T>(_: Response<T>) -> Result<(), DeserializeBodyError> {
    Ok(())
}

/// Sends a Discord request with a bound on the whole call: waiting for the
/// rate limiter (including Twilight's internal retries after 429), the
/// exchange and decoding. Twilight's pre-flight check, evaluated each time
/// the limiter grants a permit, stops a request once `wait` has passed or it
/// is no longer wanted; a request still waiting when it stops being wanted
/// is dropped at once. Once a request may have left, the call waits for its
/// answer rather than racing a later request to the same message.
async fn bounded<T: Unpin, U>(
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

#[cfg(test)]
mod tests;
