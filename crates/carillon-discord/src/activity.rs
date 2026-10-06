//! Work cards and typing while jobs run.
//!
//! A job is the work one message began: the message's turn and the
//! observations core queues to report what it delegated, all of which core
//! names by the message's turn, their origin. Core says which of this
//! provider's turns are queued or running, with the runtime's latest
//! snapshot; the delivery loop says how each turn ended once its answer or
//! notice is acknowledged, with the runtime's final report and the
//! observation that continues the job, if any. The monitor owns only the
//! mapping from jobs to per-job tasks. Each job's card and typing are
//! separate tasks watching the latest [`Job`], so one card's slow or
//! rate-limited Discord call never delays another job, and nothing here is
//! awaited by delivery.
//!
//! A job's card is one embed, edited as the job goes on and kept as its
//! summary when it ends. A turn's answer does not end a job an observation
//! continues, and a delivery older than the job's live turn never takes the
//! card back. A job with work worth a summary but no card gets its summary
//! on its answer instead, and that answer is its card from then on.
//!
//! Every Discord call is bounded, and its outcome is classified by what it
//! proves: a job gets at most one card, a card whose creation is unknown is
//! never followed by another, and cards are shown only while their records
//! can be read, so a restart knows which cards may exist and goes on
//! editing them, saying what it does not know.
use crate::{
    client::CoreClient,
    config::{ActivityMarkers, DiscordConfig},
};
use anyhow::{Context as _, Result};
use carillon_installation::filesystem::atomic_write;
use carillon_protocol::model::{ActiveTurn, Delivery, DeliveryOutcome, TurnActivity, TurnState};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
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

mod render;

/// How often core is asked which turns are active.
const POLL_INTERVAL: Duration = Duration::from_secs(3);
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
/// Recently settled turns, so a read taken before a settle cannot show a
/// finished turn again. Turn IDs are never reused.
const SETTLED_MEMORY: usize = 256;
/// The reports of a job's ended turns a card keeps, the latest.
const REPORTS_KEPT: usize = 8;

/// How the delivery loop saw one of a job's turns end, reported after core
/// recorded it.
#[derive(Clone, Debug)]
pub(crate) struct Outcome {
    pub(crate) delivery: Delivery,
    /// How sending it went.
    pub(crate) result: DeliveryOutcome,
    /// The answer that carries the job's summary, when the delivery
    /// attached one.
    pub(crate) hosted: Option<Card>,
}

/// The monitor's latest knowledge of one job.
#[derive(Clone, Debug, Default)]
struct Job {
    state: State,
    /// When the person asked, once a read or a delivery said.
    requested_at: Option<i64>,
    /// What the job's ended turns reported, oldest first.
    ended: Vec<Ended>,
    /// The answer that carries the job's summary, once one does.
    hosted: Option<Card>,
}

#[derive(Clone, Debug)]
struct Ended {
    turn_id: String,
    report: Option<TurnActivity>,
}

#[derive(Clone, Debug, Default)]
enum State {
    /// A card recorded before this start; core has not listed the job since.
    #[default]
    Unread,
    /// A turn of the job is queued or running.
    Active(Box<ActiveTurn>),
    /// Core no longer lists the job's turn, and how it ended is not yet
    /// observed.
    Finishing,
    /// A turn ended, and the observation core queued to continue the job
    /// is not listed yet.
    Continuing,
    /// The job's last turn ended, and its delivery was recorded.
    Settled {
        turn_state: TurnState,
        delivery: DeliveryOutcome,
        finished_at: i64,
    },
}

impl Job {
    const fn is_active(&self) -> bool {
        matches!(
            self.state,
            State::Active(_) | State::Unread | State::Continuing
        )
    }

    const fn is_settled(&self) -> bool {
        matches!(self.state, State::Settled { .. })
    }

    /// Takes in how one of the job's turns ended. A turn the job's running
    /// turn already continues is older news: it adds its report, but the
    /// job stays as that turn shows it.
    fn end(&mut self, delivery: &Delivery, outcome: DeliveryOutcome, hosted: Option<Card>) {
        self.requested_at = Some(delivery.origin.accepted_at);
        if !self
            .ended
            .iter()
            .any(|ended| ended.turn_id == delivery.turn_id)
        {
            self.ended.push(Ended {
                turn_id: delivery.turn_id.clone(),
                report: delivery.activity.clone(),
            });
            if self.ended.len() > REPORTS_KEPT {
                self.ended.remove(0);
            }
        }
        self.hosted = hosted.or(self.hosted);
        self.state = match (&self.state, &delivery.continued_by) {
            (State::Active(turn), Some(_)) if turn.turn_id != delivery.turn_id => {
                return;
            }
            (_, Some(_)) => State::Continuing,
            (_, None) => State::Settled {
                turn_state: delivery.turn_state,
                delivery: outcome,
                finished_at: delivery.finished_at,
            },
        };
    }

    /// The card's embed: `last` is the latest snapshot the job's running
    /// turn gave, kept while a read has none; `restarted` when the card was
    /// recorded before this start and nothing since has said anything of
    /// the job.
    fn embed(
        &self,
        last: Option<&(String, TurnActivity)>,
        restarted: bool,
        markers: &ActivityMarkers,
    ) -> Embed {
        use render::Status;
        let (status, live) = match &self.state {
            State::Active(turn) if turn.state == TurnState::Queued => (
                Status::Queued {
                    since: turn.accepted_at,
                },
                None,
            ),
            State::Active(turn) => (
                Status::Working(
                    turn.activity
                        .as_ref()
                        .or(last.map(|(_, snapshot)| snapshot))
                        .map(|snapshot| snapshot.phase),
                ),
                turn.activity.as_ref(),
            ),
            State::Unread | State::Finishing if restarted => (Status::Restarted, None),
            State::Unread | State::Finishing => (Status::Finishing, None),
            State::Continuing => (Status::Continuing, None),
            State::Settled {
                turn_state,
                delivery,
                finished_at,
            } => (
                Status::Ended {
                    turn_state: *turn_state,
                    delivery: *delivery,
                    finished_at: *finished_at,
                },
                None,
            ),
        };
        // A snapshot is history when its turn gave no final report and is
        // not the one shown running now.
        let history = last.filter(|(turn_id, _)| {
            live.is_none()
                && !self
                    .ended
                    .iter()
                    .any(|ended| &ended.turn_id == turn_id && ended.report.is_some())
        });
        let reports = self
            .ended
            .iter()
            .filter_map(|ended| ended.report.as_ref())
            .chain(history.map(|(_, snapshot)| snapshot))
            .collect();
        render::card(
            &render::Shown {
                status,
                requested_at: self.requested_at,
                reports,
                live,
            },
            markers,
        )
    }
}

/// The Discord message showing a job's work. A `hosted` card is an answer
/// that carries the job's summary: it is edited, never removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Card {
    pub(crate) channel_id: Id<ChannelMarker>,
    pub(crate) message_id: Id<MessageMarker>,
    pub(crate) hosted: bool,
}

/// What a later start must know of a job's card: the card, or, when its
/// creation was never confirmed, that a card may exist which nobody can name.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Recorded {
    channel_id: Id<ChannelMarker>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message_id: Option<Id<MessageMarker>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    hosted: bool,
}

impl Recorded {
    const fn card(self) -> Option<Card> {
        match self.message_id {
            Some(message_id) => Some(Card {
                channel_id: self.channel_id,
                message_id,
                hosted: self.hosted,
            }),
            None => None,
        }
    }
}

impl From<Card> for Recorded {
    fn from(card: Card) -> Self {
        Self {
            channel_id: card.channel_id,
            message_id: Some(card.message_id),
            hosted: card.hosted,
        }
    }
}

/// Whether a job may have a card made now.
#[derive(Debug, PartialEq, Eq)]
enum Reservation {
    /// The job has no card: the caller makes it, then records or releases it.
    Free,
    /// The job's card is recorded.
    Recorded(Recorded),
    /// Another maker is making the job's card now.
    Taken,
}

/// Cards of unfinished jobs, by job: a card this provider made or an answer
/// that carries a summary, until the job's summary is final. A restart goes
/// on with them. A card is recorded only after Discord confirms it: a crash
/// in between leaves one card no later start knows of. A creation whose
/// outcome is unknown is recorded without a message, so no start makes a
/// second card for the same job.
pub(crate) struct CardRecords {
    path: PathBuf,
    cards: Mutex<BTreeMap<String, Recorded>>,
    /// Jobs whose card is being made now, by the monitor or by an answer.
    reserved: Mutex<HashSet<String>>,
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
                reserved: Mutex::new(HashSet::new()),
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

    /// Lets one maker at a time make `job`'s card, and none once it has one.
    fn reserve(&self, job: &str) -> Reservation {
        let cards = lock(&self.cards);
        if let Some(recorded) = cards.get(job) {
            return Reservation::Recorded(*recorded);
        }
        if lock(&self.reserved).insert(job.to_owned()) {
            Reservation::Free
        } else {
            Reservation::Taken
        }
    }

    /// Ends a reservation that made no card.
    fn release(&self, job: &str) {
        lock(&self.reserved).remove(job);
    }

    fn insert(&self, job: &str, card: impl Into<Recorded>) {
        let card = card.into();
        self.change(|cards| cards.insert(job.to_owned(), card) != Some(card));
        self.release(job);
    }

    fn remove(&self, job: &str) {
        self.change(|cards| cards.remove(job).is_some());
    }

    /// Forgets a card nobody can name once its job has ended: it can no
    /// longer stand in the way of another, and there is nothing to finish.
    fn forget_unnamed(&self, job: &str) {
        self.change(|cards| {
            let unnamed = cards.get(job).is_some_and(|card| card.message_id.is_none());
            if unnamed {
                cards.remove(job);
                warn!(
                    job,
                    "an activity card of unknown identity may remain in Discord"
                );
            }
            unnamed
        });
    }

    fn change(&self, change: impl FnOnce(&mut BTreeMap<String, Recorded>) -> bool) {
        let mut cards = lock(&self.cards);
        if !change(&mut cards) {
            return;
        }
        let written = serde_json::to_vec_pretty(&*cards)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| Ok(atomic_write(&self.path, &bytes, 0o600)?));
        if let Err(error) = written {
            warn!(path = %self.path.display(), %error, "activity card records were not saved");
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the delivery loop needs to give an answer its job's summary.
#[derive(Clone)]
pub(crate) struct Summaries {
    records: Arc<CardRecords>,
    markers: ActivityMarkers,
}

impl Summaries {
    /// Summaries go on answers only while cards are on.
    pub(crate) fn new(records: Option<Arc<CardRecords>>, config: &DiscordConfig) -> Option<Self> {
        let records = records.filter(|_| config.activity)?;
        Some(Self {
            records,
            markers: config.activity_markers.clone(),
        })
    }

    /// The summary `delivery`'s answer carries, when the turn's report shows
    /// work and its job has no card, nor one being made: the answer is then
    /// reserved as the job's card. The caller says how sending it went with
    /// [`Self::sent`].
    pub(crate) fn reserve(&self, delivery: &Delivery) -> Option<Embed> {
        if !render::shows_work(delivery.activity.as_ref()) {
            return None;
        }
        let job = &delivery.origin.turn_id;
        if self.records.reserve(job) != Reservation::Free {
            return None;
        }
        let summary = render::summary(delivery, &self.markers);
        // Display never costs an answer: a summary Discord would refuse is left out.
        if let Err(error) = twilight_validate::embed::embed(&summary) {
            warn!(%error, "a work summary was left off its answer");
            self.records.release(job);
            return None;
        }
        Some(summary)
    }

    /// Records the answer that carries `delivery`'s summary as its job's
    /// card while the job goes on, so later turns update it; or ends the
    /// reservation when no answer carries it, or the summary is already
    /// the job's last.
    pub(crate) fn sent(&self, delivery: &Delivery, card: Option<Card>) {
        let job = &delivery.origin.turn_id;
        match card {
            Some(card) if delivery.continued_by.is_some() => self.records.insert(job, card),
            _ => self.records.release(job),
        }
    }
}

/// What every job task shares.
#[derive(Clone)]
struct Cards {
    http: Arc<DiscordHttp>,
    config: DiscordConfig,
    records: Arc<CardRecords>,
}

/// Follows this provider's jobs until `outcomes` closes. Cards recorded
/// before this start are taken up again. Without readable `records` there
/// are no cards, only typing.
pub(crate) async fn activity_monitor(
    core: CoreClient,
    http: Arc<DiscordHttp>,
    config: DiscordConfig,
    records: Option<Arc<CardRecords>>,
    mut outcomes: mpsc::UnboundedReceiver<Outcome>,
) {
    let typing = config.typing.then(|| http.clone());
    let cards = records.map(|records| Cards {
        http,
        config,
        records,
    });
    let mut tasks = JoinSet::new();
    let (reads_sender, mut reads) = mpsc::channel(1);
    tasks.spawn(poll_core(core, reads_sender));
    let mut jobs = HashMap::new();
    let track = |tasks: &mut JoinSet<()>, job_id: &str, job: Job, old: Option<Recorded>| {
        let (view, watched) = watch::channel(job);
        if let Some(http) = &typing {
            tasks.spawn(keep_typing(http.clone(), watched.clone()));
        }
        if let Some(cards) = &cards {
            tasks.spawn(show_card(cards.clone(), job_id.to_owned(), watched, old));
        }
        view
    };
    for (job_id, card) in cards.iter().flat_map(|cards| cards.records.recorded()) {
        let view = track(&mut tasks, &job_id, Job::default(), Some(card));
        jobs.insert(job_id, view);
    }
    let mut settled = VecDeque::new();
    let mut reachable = true;
    loop {
        tokio::select! {
            biased;
            outcome = outcomes.recv() => {
                let Some(Outcome { delivery, result, hosted }) = outcome else { return };
                if settled.len() == SETTLED_MEMORY {
                    settled.pop_front();
                }
                settled.push_back(delivery.turn_id.clone());
                let job_id = delivery.origin.turn_id.clone();
                if let Some(view) = jobs.get(&job_id) {
                    view.send_modify(|job| job.end(&delivery, result, hosted));
                    if view.borrow().is_settled() {
                        jobs.remove(&job_id);
                    }
                } else if delivery.continued_by.is_some() {
                    // A job no read has shown yet goes on: it is followed from here.
                    let mut job = Job::default();
                    job.end(&delivery, result, hosted);
                    let view = track(&mut tasks, &job_id, job, None);
                    jobs.insert(job_id, view);
                }
            }
            Some(read) = reads.recv() => {
                let active = match read {
                    Ok(active) => active,
                    // Unknown is not empty: every card keeps what it shows.
                    Err(failure) => {
                        if reachable {
                            warn!(error = %failure.message, "cannot read active turns from core");
                        }
                        reachable = false;
                        continue;
                    }
                };
                reachable = true;
                let mut listed = HashSet::new();
                for turn in active {
                    if settled.contains(&turn.turn_id) {
                        continue;
                    }
                    let job_id = turn.origin.turn_id.clone();
                    let requested_at = turn.origin.accepted_at;
                    listed.insert(job_id.clone());
                    if let Some(view) = jobs.get(&job_id) {
                        view.send_modify(|job| {
                            job.requested_at = Some(requested_at);
                            job.state = State::Active(Box::new(turn));
                        });
                    } else {
                        let job = Job {
                            state: State::Active(Box::new(turn)),
                            requested_at: Some(requested_at),
                            ..Job::default()
                        };
                        let view = track(&mut tasks, &job_id, job, None);
                        jobs.insert(job_id, view);
                    }
                }
                // Every read that still misses a job's turn says so again, so
                // a card whose edit failed is tried again.
                for (job_id, view) in &jobs {
                    if !listed.contains(job_id) {
                        view.send_modify(|job| {
                            if matches!(job.state, State::Active(_) | State::Unread) {
                                job.state = State::Finishing;
                            }
                        });
                    }
                }
                // A job whose tasks have ended is forgotten; a job stays
                // tracked while any of its tasks runs, so it never gets a
                // second card.
                jobs.retain(|_, view| !view.is_closed());
                while tasks.try_join_next().is_some() {}
            }
        }
    }
}

async fn poll_core(
    core: CoreClient,
    reads: mpsc::Sender<Result<Vec<ActiveTurn>, crate::InterfaceError>>,
) {
    let mut ticks = interval(POLL_INTERVAL);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticks.tick().await;
        if reads.send(core.active_turns().await).await.is_err() {
            return;
        }
    }
}

/// Types in a job's channel while one of its turns runs, until it ends.
async fn keep_typing(http: Arc<DiscordHttp>, mut view: watch::Receiver<Job>) {
    let mut typed: Option<Instant> = None;
    loop {
        let channel = match &view.borrow_and_update().state {
            State::Active(turn) if turn.state == TurnState::Running => turn.target_id.parse().ok(),
            State::Settled { .. } => return,
            _ => None,
        };
        if let Some(channel) = channel
            && typed.is_none_or(|at| at.elapsed() >= TYPING_REFRESH)
        {
            typed = Some(Instant::now());
            let request = http.create_typing_trigger(channel).into_future();
            if let Call::Refused(status) =
                bounded(request, PROGRESS_WAIT, Some(still_active(&view)), done).await
            {
                warn!(status, "Discord refused a typing indicator");
            }
        }
        if let Ok(Err(_)) = timeout(TYPING_REFRESH, view.changed()).await {
            return;
        }
    }
}

/// One job's card: the card recorded before this start, or one made or
/// hosted once the job is worth it, followed until the job's summary is
/// final. A card nobody can name keeps the job without one, and is
/// forgotten once the job has ended.
async fn show_card(
    cards: Cards,
    job_id: String,
    mut view: watch::Receiver<Job>,
    old: Option<Recorded>,
) {
    let card = match old {
        Some(recorded) => recorded.card(),
        None => obtain(&cards, &job_id, &mut view).await,
    };
    match card {
        Some(card) => follow(&cards, &job_id, &mut view, card, old.is_some()).await,
        None => {
            let _ = view.wait_for(|job| !job.is_active()).await;
        }
    }
    if !view.borrow().is_active() {
        cards.records.forget_unnamed(&job_id);
    }
}

/// The job's card once it is worth one: the answer that carries its
/// summary, or a card made once the job has run past the appearance delay.
/// `None` when the job ends first, or it gets no card.
async fn obtain(cards: &Cards, job_id: &str, view: &mut watch::Receiver<Job>) -> Option<Card> {
    loop {
        let job = view.borrow_and_update().clone();
        if let Some(card) = job.hosted {
            return Some(card);
        }
        if job.is_settled() {
            return None;
        }
        if let State::Active(turn) = &job.state
            && cards.config.activity
        {
            let channel = match turn.target_id.parse::<Id<ChannelMarker>>() {
                Ok(channel) => channel,
                Err(error) => {
                    warn!(%error, "activity card target is not a Discord channel");
                    return None;
                }
            };
            // Anything that changes the job before then is looked at again.
            let appear = Instant::now() + until_after(turn.accepted_at, APPEAR_AFTER);
            match timeout_at(appear, view.changed()).await {
                Ok(Ok(())) => continue,
                Ok(Err(_)) => return None,
                Err(_) => {}
            }
            match cards.records.reserve(job_id) {
                Reservation::Recorded(recorded) => return recorded.card(),
                // An answer is being given the job's summary: it says which.
                Reservation::Taken => {}
                Reservation::Free => {
                    let embed = job.embed(None, false, &cards.config.activity_markers);
                    match cards.create(channel, &embed, view).await {
                        Call::Done(message_id) => {
                            let card = Card {
                                channel_id: channel,
                                message_id,
                                hosted: false,
                            };
                            cards.records.insert(job_id, card);
                            return Some(card);
                        }
                        Call::NotSent => cards.records.release(job_id),
                        outcome @ (Call::Refused(_) | Call::Unknown) => {
                            // Discord refused it, or may hold a card this task
                            // cannot name: either way the job gets no card,
                            // even after a restart.
                            if outcome == Call::Unknown {
                                let unnamed = Recorded {
                                    channel_id: channel,
                                    message_id: None,
                                    hosted: false,
                                };
                                cards.records.insert(job_id, unnamed);
                            } else {
                                cards.records.release(job_id);
                            }
                            warn!(job_id, refused = ?outcome.refused(), "no activity card for this job");
                            return None;
                        }
                    }
                }
            }
        }
        if view.changed().await.is_err() {
            return None;
        }
    }
}

/// Edits the card as the job goes on, at most once per interval, and leaves
/// its summary when the delivery loop reports the job's last outcome. An
/// `adopted` card was recorded before this start: until something is heard
/// of its job, it says the restart left its outcome unknown here.
async fn follow(
    cards: &Cards,
    job_id: &str,
    view: &mut watch::Receiver<Job>,
    card: Card,
    adopted: bool,
) {
    let markers = &cards.config.activity_markers;
    // The snapshot the card was made from is the first one it knows.
    let mut last = match &view.borrow().state {
        State::Active(turn) => turn
            .activity
            .clone()
            .map(|snapshot| (turn.turn_id.clone(), snapshot)),
        _ => None,
    };
    let mut heard = !adopted;
    let mut shown = (!adopted && !card.hosted).then(|| view.borrow().embed(None, false, markers));
    let mut next_edit = Instant::now()
        + if adopted {
            Duration::ZERO
        } else {
            EDIT_INTERVAL
        };
    // Discord may refuse edits (403) yet allow removal.
    let mut editable = true;
    let job = loop {
        if !view.borrow().is_settled() {
            if view.changed().await.is_err() && !view.borrow().is_settled() {
                // The monitor stopped; the record lets the next start go on.
                return;
            }
            // Edits wait out the interval; the job's last outcome does not.
            let _ = timeout_at(next_edit, view.wait_for(Job::is_settled)).await;
        }
        let job = view.borrow_and_update().clone();
        if let State::Active(turn) = &job.state {
            heard = true;
            if let Some(snapshot) = &turn.activity {
                last = Some((turn.turn_id.clone(), snapshot.clone()));
            }
        }
        heard |= !job.ended.is_empty();
        if job.is_settled() {
            break job;
        }
        if matches!(job.state, State::Unread) {
            continue;
        }
        let embed = job.embed(last.as_ref(), !heard, markers);
        if !editable || shown.as_ref() == Some(&embed) {
            continue;
        }
        match cards.edit(card, &embed, Some(not_settled(view))).await {
            Call::Done(()) => shown = Some(embed),
            // The next read brings the latest again.
            Call::NotSent | Call::Unknown => {}
            Call::Refused(404) => {
                // Deleted by someone: never replace it.
                cards.records.remove(job_id);
                warn!(job_id, "activity card was deleted in Discord");
                let _ = view.wait_for(|job| !job.is_active()).await;
                return;
            }
            Call::Refused(status) => {
                warn!(job_id, status, "activity card can no longer be edited");
                editable = false;
            }
        }
        next_edit = Instant::now() + EDIT_INTERVAL;
    };
    let summary = editable.then(|| job.embed(last.as_ref(), !heard, markers));
    cards.finish(job_id, card, summary).await;
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
    /// Leaves the card showing the job's `summary`. A card that cannot show
    /// it is removed instead of left claiming work, unless it is an answer.
    async fn finish(&self, job_id: &str, card: Card, summary: Option<Embed>) {
        if summary.is_none() && card.hosted {
            self.records.remove(job_id);
            return;
        }
        for attempt in 1..=FINAL_ATTEMPTS {
            let call = match &summary {
                None => self.delete(card).await,
                Some(embed) => self.edit(card, embed, None).await,
            };
            match call {
                Call::Done(()) | Call::Refused(404) => {
                    self.records.remove(job_id);
                    return;
                }
                Call::Refused(_) => break,
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

    async fn create(
        &self,
        channel: Id<ChannelMarker>,
        embed: &Embed,
        view: &watch::Receiver<Job>,
    ) -> Call<Id<MessageMarker>> {
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
            Some(still_active(view)),
            async |response| Ok(response.model().await?.id),
        )
        .await
    }

    /// Replaces the card's embed; an answer's text stays as it is.
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
#[derive(Debug, PartialEq, Eq)]
enum Call<T> {
    Done(T),
    /// Discord certainly did not receive it.
    NotSent,
    /// Discord answered with this client error status: it had no effect.
    Refused(u16),
    /// It may or may not have taken effect.
    Unknown,
}

impl<T> Call<T> {
    const fn refused(&self) -> Option<u16> {
        match self {
            Self::Refused(status) => Some(*status),
            _ => None,
        }
    }
}

/// While a request waits for the rate limiter, the job must still want it.
struct Wanted {
    view: watch::Receiver<Job>,
    holds: fn(&Job) -> bool,
}

fn still_active(view: &watch::Receiver<Job>) -> Wanted {
    Wanted {
        view: view.clone(),
        holds: Job::is_active,
    }
}

fn not_settled(view: &watch::Receiver<Job>) -> Wanted {
    Wanted {
        view: view.clone(),
        holds: |job| !job.is_settled(),
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
        .map(|wanted| (wanted.view.clone(), wanted.holds));
    let preflight = request.set_pre_flight(move || {
        let proceed = Instant::now() < deadline
            && check
                .as_ref()
                .is_none_or(|(view, holds)| holds(&view.borrow()));
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
    match wanted {
        Some(Wanted { mut view, holds }) => {
            let _ = view.wait_for(|view| !holds(view)).await;
        }
        None => pending().await,
    }
}

#[cfg(test)]
mod tests;
