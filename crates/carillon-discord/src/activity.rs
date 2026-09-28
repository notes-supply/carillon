//! Temporary activity cards and typing while turns run.
//!
//! Core says which of this provider's turns are queued or running, with the
//! runtime's latest snapshot; the delivery loop says how each turn ended once
//! its answer or notice is acknowledged. The monitor owns only the mapping
//! from turns to per-turn tasks. Each turn's card and typing are separate
//! tasks watching the latest [`View`] of that turn, so one card's slow or
//! rate-limited Discord call never delays another turn, and nothing here is
//! awaited by delivery.
//!
//! Every Discord call is bounded, and its outcome is classified by what it
//! proves: a card is created at most once per turn, and a card whose cleanup
//! cannot be established is never replaced. Cards are shown only while their
//! records can be read, so a restart knows which old cards may exist.
use crate::{client::CoreClient, config::DiscordConfig};
use anyhow::{Context as _, Result};
use carillon_installation::filesystem::atomic_write;
use carillon_protocol::model::{
    ActiveTurn, ActivityPhase, DeliveryOutcome, FinishedTool, TurnActivity, TurnState,
};
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
    channel::message::AllowedMentions,
    id::{
        Id,
        marker::{ChannelMarker, MessageMarker},
    },
};

/// How often core is asked which turns are active.
const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// A turn that finishes sooner after acceptance never shows a card.
const APPEAR_AFTER: Duration = Duration::from_secs(2);
/// The least time between two edits of one card.
const EDIT_INTERVAL: Duration = Duration::from_secs(3);
/// Discord shows typing for ten seconds; refresh before it lapses.
const TYPING_REFRESH: Duration = Duration::from_secs(8);
/// How long a request may wait for Discord's rate limiter before it is no
/// longer worth sending. Progress is superseded by the next read; a card's
/// creation and removal are worth waiting longer for.
const PROGRESS_WAIT: Duration = Duration::from_secs(3);
const CARD_WAIT: Duration = Duration::from_secs(10);
/// Once sent, Twilight's own 10-second HTTP timeout and response decoding.
const EXCHANGE_LIMIT: Duration = Duration::from_secs(12);
/// Attempts to remove or finalize a card before leaving it to the next start.
const FINAL_ATTEMPTS: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// Recently settled turns, so a read taken before a settle cannot show a
/// finished turn again. Turn IDs are never reused.
const SETTLED_MEMORY: usize = 256;
const RECENT_LINES: usize = 5;
const RUNNING_LINES: usize = 3;
const DETAIL_LIMIT: usize = 120;
const HEADLINE_LIMIT: usize = 100;
const MESSAGE_LIMIT: usize = 2_000;

/// How the delivery loop saw a turn end, reported after core recorded it.
#[derive(Clone, Debug)]
pub(crate) struct Outcome {
    pub(crate) turn_id: String,
    pub(crate) turn_state: TurnState,
    pub(crate) delivery: DeliveryOutcome,
}

/// The monitor's latest knowledge of one turn.
#[derive(Clone, Debug)]
enum View {
    /// A card recorded before this start; core has not listed the turn yet.
    Unread,
    Active(ActiveTurn),
    /// Core no longer lists the turn, and its delivery is not yet observed.
    Finishing,
    Settled {
        turn_state: TurnState,
        delivery: DeliveryOutcome,
    },
}

impl View {
    const fn is_active(&self) -> bool {
        matches!(self, Self::Active(_) | Self::Unread)
    }

    const fn is_settled(&self) -> bool {
        matches!(self, Self::Settled { .. })
    }
}

/// The Discord message showing a turn's activity.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Card {
    channel_id: Id<ChannelMarker>,
    message_id: Id<MessageMarker>,
}

/// What a later start must know of a turn's card: the card, or, when its
/// creation was never confirmed, that a card may exist which nobody can name.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Recorded {
    channel_id: Id<ChannelMarker>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message_id: Option<Id<MessageMarker>>,
}

impl From<Card> for Recorded {
    fn from(card: Card) -> Self {
        Self {
            channel_id: card.channel_id,
            message_id: Some(card.message_id),
        }
    }
}

/// Cards this provider created and has not yet removed or finalized, by
/// turn, so a restart can remove them. A card is recorded only after Discord
/// confirms it: a crash in between leaves one card no later start knows of.
/// A creation whose outcome is unknown is recorded without a message, so a
/// restart does not create a second card for the same running turn.
pub(crate) struct CardRecords {
    path: PathBuf,
    cards: Mutex<BTreeMap<String, Recorded>>,
}

impl CardRecords {
    /// Opens the records, or returns `None` when they cannot be read: without
    /// them nobody knows which old cards exist, so cards stay off for this
    /// start rather than risk replacements. The file is left as it is for
    /// the operator. An absent file is simply no records.
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
        self.cards
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn insert(&self, turn_id: &str, card: impl Into<Recorded>) {
        let card = card.into();
        self.change(|cards| cards.insert(turn_id.to_owned(), card) != Some(card));
    }

    fn remove(&self, turn_id: &str) {
        self.change(|cards| cards.remove(turn_id).is_some());
    }

    /// Forgets a card nobody can name once its turn has ended: it can no
    /// longer cause a replacement, and there is nothing to remove.
    fn forget_unnamed(&self, turn_id: &str) {
        self.change(|cards| {
            let unnamed = cards
                .get(turn_id)
                .is_some_and(|card| card.message_id.is_none());
            if unnamed {
                cards.remove(turn_id);
                warn!(
                    turn_id,
                    "an activity card of unknown identity may remain in Discord"
                );
            }
            unnamed
        });
    }

    fn change(&self, change: impl FnOnce(&mut BTreeMap<String, Recorded>) -> bool) {
        let mut cards = self.cards.lock().unwrap_or_else(PoisonError::into_inner);
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

/// What every turn task shares.
#[derive(Clone)]
struct Cards {
    http: Arc<DiscordHttp>,
    config: DiscordConfig,
    records: Arc<CardRecords>,
}

/// Follows this provider's active turns until `outcomes` closes. Cards
/// recorded before this start are removed before any replacement is shown.
/// Without readable `records` there are no cards, only typing.
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
    let mut turns = HashMap::new();
    for (turn_id, card) in cards.iter().flat_map(|cards| cards.records.recorded()) {
        let (view, watched) = watch::channel(View::Unread);
        spawn_turn(
            &mut tasks,
            typing.as_ref(),
            cards.as_ref(),
            &turn_id,
            watched,
            Some(card),
        );
        turns.insert(turn_id, view);
    }
    let mut settled = VecDeque::new();
    let mut reachable = true;
    loop {
        tokio::select! {
            biased;
            outcome = outcomes.recv() => {
                let Some(outcome) = outcome else { return };
                if let Some(view) = turns.remove(&outcome.turn_id) {
                    view.send_replace(View::Settled {
                        turn_state: outcome.turn_state,
                        delivery: outcome.delivery,
                    });
                }
                if settled.len() == SETTLED_MEMORY {
                    settled.pop_front();
                }
                settled.push_back(outcome.turn_id);
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
                let listed = active.iter().map(|turn| turn.turn_id.clone()).collect::<HashSet<_>>();
                for turn in active {
                    if settled.contains(&turn.turn_id) {
                        continue;
                    }
                    if let Some(view) = turns.get(&turn.turn_id) {
                        view.send_replace(View::Active(turn));
                    } else {
                        let turn_id = turn.turn_id.clone();
                        let (view, watched) = watch::channel(View::Active(turn));
                        let (typing, cards) = (typing.as_ref(), cards.as_ref());
                        spawn_turn(&mut tasks, typing, cards, &turn_id, watched, None);
                        turns.insert(turn_id, view);
                    }
                }
                // Every read that still misses a turn says so again, so a
                // card whose finishing edit failed is tried again.
                for (turn_id, view) in &turns {
                    if !listed.contains(turn_id) {
                        view.send_replace(View::Finishing);
                    }
                }
                // A turn whose tasks have ended is forgotten; a turn stays
                // tracked while any of its tasks runs, so it is never shown twice.
                turns.retain(|_, view| !view.is_closed());
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

fn spawn_turn(
    tasks: &mut JoinSet<()>,
    typing: Option<&Arc<DiscordHttp>>,
    cards: Option<&Cards>,
    turn_id: &str,
    view: watch::Receiver<View>,
    old: Option<Recorded>,
) {
    if let Some(http) = typing {
        tasks.spawn(keep_typing(http.clone(), view.clone()));
    }
    if let Some(cards) = cards {
        tasks.spawn(show_card(cards.clone(), turn_id.to_owned(), view, old));
    }
}

/// Types in a running turn's channel until the turn leaves core's list.
async fn keep_typing(http: Arc<DiscordHttp>, mut view: watch::Receiver<View>) {
    let mut typed: Option<Instant> = None;
    loop {
        let channel = match &*view.borrow_and_update() {
            View::Active(turn) if turn.state == TurnState::Running => turn.target_id.parse().ok(),
            View::Active(_) | View::Unread => None,
            View::Finishing | View::Settled { .. } => return,
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

/// One turn's card: remove a card left by an earlier start, then show the
/// turn's card unless an earlier card may still exist. A card nobody can name
/// is forgotten once its turn has ended.
async fn show_card(
    cards: Cards,
    turn_id: String,
    mut view: watch::Receiver<View>,
    old: Option<Recorded>,
) {
    let cleared = match old {
        None => true,
        Some(Recorded {
            channel_id,
            message_id: Some(message_id),
        }) => {
            let card = Card {
                channel_id,
                message_id,
            };
            cards.sweep(&turn_id, card).await
        }
        Some(Recorded {
            message_id: None, ..
        }) => false,
    };
    let turn = loop {
        match &*view.borrow_and_update() {
            View::Active(turn) => break Some(turn.clone()),
            View::Unread => {}
            View::Finishing | View::Settled { .. } => break None,
        }
        if view.changed().await.is_err() {
            return;
        }
    };
    let channel = turn.map(|turn| (turn.target_id.parse::<Id<ChannelMarker>>(), turn));
    match channel {
        Some((Ok(channel), turn)) if cleared && cards.config.activity => {
            follow_card(&cards, &turn_id, &mut view, channel, &turn).await;
        }
        Some((target, _)) => {
            if let Err(error) = target {
                warn!(%error, "activity card target is not a Discord channel");
            }
            // Stay tracked, so the turn never gets a card while active.
            let _ = view.wait_for(|view| !view.is_active()).await;
        }
        None => {}
    }
    if !view.borrow().is_active() {
        cards.records.forget_unnamed(&turn_id);
    }
}

/// Waits out the appearance delay, creates the card once, edits it as the
/// turn changes, and removes or finalizes it when the delivery loop reports
/// the outcome.
async fn follow_card(
    cards: &Cards,
    turn_id: &str,
    view: &mut watch::Receiver<View>,
    channel: Id<ChannelMarker>,
    turn: &ActiveTurn,
) {
    let appear = Instant::now() + until_after(turn.accepted_at, APPEAR_AFTER);
    if timeout_at(appear, view.wait_for(|view| !view.is_active()))
        .await
        .is_ok()
    {
        return;
    }
    let mut last = None;
    let (card, mut shown) = loop {
        let View::Active(turn) = view.borrow_and_update().clone() else {
            return;
        };
        let text = active_text(&turn, &mut last);
        match cards.create(channel, &text, view).await {
            Call::Done(message_id) => {
                let card = Card {
                    channel_id: channel,
                    message_id,
                };
                break (card, text);
            }
            Call::NotSent => {
                if view.changed().await.is_err() {
                    return;
                }
            }
            outcome @ (Call::Refused(_) | Call::Unknown) => {
                // Discord refused it, or may hold a card this task cannot
                // name: either way the turn gets no card, even after a restart.
                if outcome == Call::Unknown {
                    let unnamed = Recorded {
                        channel_id: channel,
                        message_id: None,
                    };
                    cards.records.insert(turn_id, unnamed);
                }
                warn!(turn_id, refused = ?outcome.refused(), "no activity card for this turn");
                let _ = view.wait_for(|view| !view.is_active()).await;
                return;
            }
        }
    };
    cards.records.insert(turn_id, card);
    let mut next_edit = Instant::now() + EDIT_INTERVAL;
    // Discord may refuse edits (403) yet allow removal; only removal remains then.
    let mut editable = true;
    let (turn_state, delivery) = loop {
        if !view.borrow().is_settled() {
            if view.changed().await.is_err() && !view.borrow().is_settled() {
                // The monitor stopped; the record lets the next start remove it.
                return;
            }
            // Edits wait out the interval; the outcome does not.
            let _ = timeout_at(next_edit, view.wait_for(View::is_settled)).await;
        }
        let text = match view.borrow_and_update().clone() {
            View::Settled {
                turn_state,
                delivery,
            } => break (turn_state, delivery),
            View::Active(turn) => active_text(&turn, &mut last),
            View::Finishing => finishing_text(last.as_ref()),
            View::Unread => continue,
        };
        if !editable || text == shown {
            continue;
        }
        match cards.edit(card, &text, Some(not_settled(view))).await {
            Call::Done(()) => shown = text,
            // The next read brings the latest text again.
            Call::NotSent | Call::Unknown => {}
            Call::Refused(404) => {
                // Deleted by someone: never replace it.
                cards.records.remove(turn_id);
                warn!(turn_id, "activity card was deleted in Discord");
                let _ = view.wait_for(|view| !view.is_active()).await;
                return;
            }
            Call::Refused(status) => {
                warn!(turn_id, status, "activity card can no longer be edited");
                editable = false;
            }
        }
        next_edit = Instant::now() + EDIT_INTERVAL;
    };
    let status = if editable {
        final_text(turn_state, delivery, last.as_ref())
    } else {
        // A card that cannot say how the turn ended is removed instead of
        // left claiming work.
        None
    };
    cards.finish(turn_id, card, status).await;
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
    /// Removes a card left by an earlier start. Only a confirmed removal (or
    /// a card already gone) clears the turn for a replacement; otherwise the
    /// record stays as evidence the card may still exist.
    async fn sweep(&self, turn_id: &str, card: Card) -> bool {
        for attempt in 1..=FINAL_ATTEMPTS {
            match self.delete(card).await {
                Call::Done(()) | Call::Refused(404) => {
                    self.records.remove(turn_id);
                    return true;
                }
                Call::Refused(status) => {
                    warn!(
                        turn_id,
                        status, "Discord refused to remove an earlier activity card"
                    );
                    return false;
                }
                Call::NotSent | Call::Unknown if attempt < FINAL_ATTEMPTS => {
                    sleep(RETRY_DELAY).await;
                }
                Call::NotSent | Call::Unknown => {}
            }
        }
        warn!(
            turn_id,
            "an earlier activity card could not be removed; it stays recorded"
        );
        false
    }

    /// Leaves the card saying `status`, or removes it without one.
    async fn finish(&self, turn_id: &str, card: Card, kept: Option<String>) {
        for attempt in 1..=FINAL_ATTEMPTS {
            let call = match &kept {
                None => self.delete(card).await,
                Some(text) => self.edit(card, text, None).await,
            };
            match call {
                Call::Done(()) | Call::Refused(404) => {
                    self.records.remove(turn_id);
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
            turn_id,
            "activity card was not finalized; the next start removes it"
        );
    }

    async fn create(
        &self,
        channel: Id<ChannelMarker>,
        text: &str,
        view: &watch::Receiver<View>,
    ) -> Call<Id<MessageMarker>> {
        let silent = AllowedMentions::default();
        let request = self
            .http
            .create_message(channel)
            .allowed_mentions(Some(&silent))
            .content(text)
            .into_future();
        bounded(
            request,
            CARD_WAIT,
            Some(still_active(view)),
            async |response| Ok(response.model().await?.id),
        )
        .await
    }

    async fn edit(&self, card: Card, text: &str, wanted: Option<Wanted>) -> Call<()> {
        let silent = AllowedMentions::default();
        let request = self
            .http
            .update_message(card.channel_id, card.message_id)
            .allowed_mentions(Some(&silent))
            .content(Some(text))
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

/// While a request waits for the rate limiter, the view must still want it.
struct Wanted {
    view: watch::Receiver<View>,
    holds: fn(&View) -> bool,
}

fn still_active(view: &watch::Receiver<View>) -> Wanted {
    Wanted {
        view: view.clone(),
        holds: View::is_active,
    }
}

fn not_settled(view: &watch::Receiver<View>) -> Wanted {
    Wanted {
        view: view.clone(),
        holds: |view| !view.is_settled(),
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

/// A running or queued turn's card. Remembers the latest snapshot so a turn
/// the runtime cannot describe right now keeps its recent history, without
/// claiming anything is still running.
fn active_text(turn: &ActiveTurn, last: &mut Option<TurnActivity>) -> String {
    let started = format!("<t:{}:R>", turn.accepted_at);
    if turn.state == TurnState::Queued {
        return format!("⏳ Waiting for earlier work to finish · queued {started}");
    }
    if let Some(activity) = &turn.activity {
        *last = Some(activity.clone());
    }
    let Some(activity) = &turn.activity else {
        return card_text(
            &format!("⏳ Working · started {started}"),
            last.as_ref(),
            false,
        );
    };
    let label = match activity.phase {
        ActivityPhase::Starting => " · Starting",
        ActivityPhase::Model => " · Waiting for the model",
        ActivityPhase::Tools => "",
        ActivityPhase::Retrying => " · Retrying the model request",
        ActivityPhase::Compacting => " · Compacting the conversation",
    };
    card_text(
        &format!("⏳ Working{label} · started {started}"),
        Some(activity),
        true,
    )
}

/// Core has stopped listing the turn: it no longer runs, and its answer or
/// notice is on its way. Nothing is shown as still running.
fn finishing_text(last: Option<&TurnActivity>) -> String {
    card_text("⏳ Finishing up", last, false)
}

/// What a card says after its turn ended, or `None` when it is removed.
fn final_text(
    turn_state: TurnState,
    delivery: DeliveryOutcome,
    last: Option<&TurnActivity>,
) -> Option<String> {
    let status = match (turn_state, delivery) {
        (TurnState::Completed, DeliveryOutcome::Delivered) => return None,
        (TurnState::Completed, DeliveryOutcome::Uncertain) => {
            "⚠️ Finished, but the reply may not have been delivered."
        }
        (_, DeliveryOutcome::Delivered) => "⏹️ Stopped before finishing.",
        (_, DeliveryOutcome::Uncertain) => {
            "⏹️ Stopped before finishing; the notice about it may not have been delivered."
        }
    };
    Some(card_text(status, last, false))
}

fn card_text(header: &str, activity: Option<&TurnActivity>, running: bool) -> String {
    let mut lines = vec![header.to_owned()];
    if let Some(activity) = activity {
        if running && let Some(headline) = &activity.headline {
            lines.push(format!("> {}", cut(headline, HEADLINE_LIMIT)));
        }
        let shown_running = if running { activity.running.len() } else { 0 };
        let earlier = usize::try_from(activity.tool_calls)
            .unwrap_or(usize::MAX)
            .saturating_sub(activity.recent.len() + shown_running);
        if earlier > 0 {
            lines.push(format!(
                "… {earlier} earlier call{}",
                if earlier == 1 { "" } else { "s" }
            ));
        }
        let recent = activity.recent.len().saturating_sub(RECENT_LINES);
        for FinishedTool { tool, detail, ok } in &activity.recent[recent..] {
            lines.push(tool_line(
                if *ok { "✓" } else { "✗" },
                tool,
                detail.as_deref(),
            ));
        }
        if running {
            for tool in activity.running.iter().take(RUNNING_LINES) {
                lines.push(format!(
                    "{} · <t:{}:R>",
                    tool_line("↳", &tool.tool, tool.detail.as_deref()),
                    tool.started_at
                ));
            }
            let more = activity.running.len().saturating_sub(RUNNING_LINES);
            if more > 0 {
                lines.push(format!("↳ … and {more} more running"));
            }
        }
    }
    cut(&lines.join("\n"), MESSAGE_LIMIT)
}

/// A tool as plain text and its detail as inline code, so neither can
/// format, mention or link anything.
fn tool_line(mark: &str, tool: &str, detail: Option<&str>) -> String {
    let tool = tool
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || "_.:-".contains(character) {
                character
            } else {
                '?'
            }
        })
        .collect::<String>();
    match detail {
        Some(detail) => format!(
            "{mark} {tool} `{}`",
            cut(&detail.replace('`', "ˋ"), DETAIL_LIMIT)
        ),
        None => format!("{mark} {tool}"),
    }
}

fn cut(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit - 1) {
        Some((end, _)) if text[end..].chars().count() > 1 => format!("{}…", &text[..end]),
        _ => text.to_owned(),
    }
}

#[cfg(test)]
mod tests;
