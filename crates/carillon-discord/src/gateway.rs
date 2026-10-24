//! The Discord gateway session, and resuming it across a short graceful
//! restart.
//!
//! Twilight advances a session's sequence as each dispatch arrives, before
//! Carillon has handled it: a message the gateway takes is only queued, and
//! core accepts it later, if at all. A resume therefore never starts from the
//! received sequence. It starts from the replay floor: just below the earliest
//! message of the current session this process took or saw without settling,
//! or the latest sequence received when every one is settled. A new session,
//! which restarts sequences, leaves the old session's messages to their
//! preparations: no resume of the new session can replay them. Discord replays what follows; core's
//! event identity resolves the messages it already accepted, and an
//! interaction already acknowledged cannot be acknowledged again, so it is
//! never run twice. Anything else replayed is admitted afresh, under the
//! settings serving then.
//!
//! A graceful stop asks Discord to keep the session resumable, closes the
//! connection, and leaves one owner-private record of the session for the
//! next start, which consumes it before use. The record supports only
//! Discord's short resume window; anything that makes it unusable starts a
//! new session, and the log names why.

use std::{
    collections::BTreeSet,
    fmt, fs,
    io::Read as _,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use carillon_installation::filesystem::{PublishError, publish, remove};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::{sync::mpsc, time::timeout};
use tracing::{info, warn};
use twilight_gateway::{
    CloseFrame, ConfigBuilder, Event, EventTypeFlags, Intents, Session, Shard, ShardId,
    StreamExt as _, error::ReceiveMessageError,
};

/// The dispatches the provider handles, and the session facts it reports.
const EVENTS: EventTypeFlags = EventTypeFlags::MESSAGE_CREATE
    .union(EventTypeFlags::INTERACTION_CREATE)
    .union(EventTypeFlags::REACTION_ADD)
    .union(EventTypeFlags::READY)
    .union(EventTypeFlags::RESUMED)
    .union(EventTypeFlags::GATEWAY_INVALIDATE_SESSION);
const INTENTS: Intents = Intents::GUILDS
    .union(Intents::GUILD_MESSAGES)
    .union(Intents::GUILD_MESSAGE_REACTIONS)
    .union(Intents::MESSAGE_CONTENT);
/// How long a stop waits for Discord to close the connection.
const CLOSE_BOUND: Duration = Duration::from_secs(5);
/// The oldest record a start resumes. Discord keeps a session resumable
/// only briefly; an older one is not tried.
const RECORD_AGE: Duration = Duration::from_mins(10);
/// A record is a few hundred bytes; anything larger is not one.
const RECORD_LIMIT: u64 = 4096;
/// Messages taken without being settled that a record can still cover.
const UNSETTLED_LIMIT: usize = 1024;

/// A dispatch of one gateway session of this process. Sequences restart
/// with each new session, so a sequence alone does not name a dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Dispatch {
    /// Which of this process's sessions, counted from its start.
    session: u64,
    sequence: u64,
}

#[cfg(test)]
impl Dispatch {
    pub(crate) const fn new(session: u64, sequence: u64) -> Self {
        Self { session, sequence }
    }

    pub(crate) const fn sequence(self) -> u64 {
        self.sequence
    }
}

/// Who may resume a recorded session: the provider, its bot and guild, and
/// the token it connects with, which is kept only as a digest.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Identity {
    provider: String,
    bot_user_id: String,
    guild_id: String,
    token_sha256: String,
}

impl Identity {
    pub(crate) fn new(provider: &str, bot_user_id: &str, guild_id: &str, token: &str) -> Self {
        Self {
            provider: provider.to_owned(),
            bot_user_id: bot_user_id.to_owned(),
            guild_id: guild_id.to_owned(),
            token_sha256: hex::encode(Sha256::digest(token.as_bytes())),
        }
    }
}

/// A Discord gateway address the session's credentials may be sent to: a
/// `wss` host of Discord's gateway, with nothing else.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ResumeAddress(String);

impl ResumeAddress {
    fn parse(address: &str) -> Option<Self> {
        let host = address.strip_prefix("wss://")?;
        let labels_valid = host.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        });
        (labels_valid && host.len() <= 253 && host.ends_with(".discord.gg"))
            .then(|| Self(address.to_owned()))
    }

    /// Any address, for a local gateway in tests.
    #[cfg(test)]
    pub(crate) fn unchecked(address: String) -> Self {
        Self(address)
    }
}

/// A session id as Discord issues them.
fn session_id_valid(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

/// What a graceful stop leaves for the next start.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    identity: Identity,
    session_id: String,
    /// The replay floor: every dispatch up to it was settled.
    sequence: u64,
    resume_url: String,
    /// Seconds since the Unix epoch.
    recorded_at: u64,
}

/// How a start connects to the gateway.
#[derive(Debug)]
pub(crate) enum Start {
    /// Resume the recorded session from its replay floor.
    Resume {
        session: Session,
        address: ResumeAddress,
    },
    /// Identify anew; messages sent while the provider was away are not
    /// replayed, for this reason.
    Identify(Gap),
}

/// Why a start does not resume a session.
#[derive(Debug)]
pub(crate) enum Gap {
    /// No graceful stop left a record.
    NoRecord,
    /// The record could not be read.
    Unreadable(std::io::Error),
    /// The record could not be removed, so it is not used: using it again
    /// at a later start would replay its messages twice.
    NotConsumed(PublishError),
    /// The record is not one this provider writes.
    Corrupt,
    /// The record belongs to another provider, bot, guild or token.
    OtherIdentity,
    /// The record is older than Discord keeps sessions resumable, or from
    /// the future.
    Stale,
    /// The record names a resume address that is not Discord's gateway.
    UnsafeAddress,
}

impl fmt::Display for Gap {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRecord => formatter.write_str("no resumable session was recorded"),
            Self::Unreadable(error) => {
                write!(formatter, "the recorded session could not be read: {error}")
            }
            Self::NotConsumed(error) => {
                write!(
                    formatter,
                    "the recorded session could not be removed ({error})"
                )
            }
            Self::Corrupt => formatter.write_str("the recorded session is not valid"),
            Self::OtherIdentity => {
                formatter.write_str("the recorded session belongs to another bot, guild or token")
            }
            Self::Stale => formatter.write_str("the recorded session is too old to resume"),
            Self::UnsafeAddress => {
                formatter.write_str("the recorded resume address is not Discord's gateway")
            }
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Consumes the session record at `path`, if any, and decides how to start:
/// the record is removed before it is used, so no later start uses it again.
pub(crate) fn take_record(path: &Path, identity: &Identity) -> Start {
    let found = match read_record(path) {
        Ok(Some(found)) => found,
        Ok(None) => return Start::Identify(Gap::NoRecord),
        Err(error) => return Start::Identify(Gap::Unreadable(error)),
    };
    match remove(path) {
        Ok(()) => {}
        Err(PublishError::Unsynced(error)) => {
            warn!(error = %error, "the recorded Discord session was removed, but a system crash may bring it back");
        }
        Err(error) => return Start::Identify(Gap::NotConsumed(error)),
    }
    let Found::Record(bytes) = found else {
        return Start::Identify(Gap::Corrupt);
    };
    let Ok(record) = serde_json::from_slice::<Record>(&bytes) else {
        return Start::Identify(Gap::Corrupt);
    };
    if record.identity != *identity {
        return Start::Identify(Gap::OtherIdentity);
    }
    match now().checked_sub(record.recorded_at) {
        Some(age) if age <= RECORD_AGE.as_secs() => {}
        _ => return Start::Identify(Gap::Stale),
    }
    if !session_id_valid(&record.session_id) {
        return Start::Identify(Gap::Corrupt);
    }
    let Some(address) = ResumeAddress::parse(&record.resume_url) else {
        return Start::Identify(Gap::UnsafeAddress);
    };
    Start::Resume {
        session: Session::new(record.sequence, record.session_id),
        address,
    }
}

/// What the record's path holds.
enum Found {
    /// A regular file within [`RECORD_LIMIT`].
    Record(Vec<u8>),
    /// Something no stop writes: another kind of file, or a larger one.
    NotARecord,
}

/// What the record's path holds, `None` when nothing.
fn read_record(path: &Path) -> std::io::Result<Option<Found>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Ok(Some(Found::NotARecord));
    }
    // One byte past the limit is read, so a larger file, even one that grew
    // since, is never taken for a record.
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(RECORD_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    Ok(Some(
        if u64::try_from(bytes.len()).is_ok_and(|length| length <= RECORD_LIMIT) {
            Found::Record(bytes)
        } else {
            Found::NotARecord
        },
    ))
}

/// The provider's gateway connection, and the floor below which every
/// dispatch of its current session it took is settled.
pub(crate) struct Gateway {
    shard: Shard,
    /// The current session, counted from this process's start: a READY
    /// begins the next.
    session: u64,
    /// Messages of the current session taken or seen without being settled,
    /// by sequence.
    unsettled: BTreeSet<u64>,
    /// More messages of the current session went unsettled than a record
    /// can cover.
    overflowed: bool,
    settlements: mpsc::UnboundedReceiver<Dispatch>,
    /// Whether this connection set out to resume a recorded session.
    resuming: bool,
}

impl Gateway {
    /// Connects as `start` says, with settled messages reported on
    /// `settlements`.
    pub(crate) fn new(
        token: String,
        start: Start,
        settlements: mpsc::UnboundedReceiver<Dispatch>,
    ) -> Self {
        Self::configured(ConfigBuilder::new(token, INTENTS), start, settlements)
    }

    fn configured(
        config: ConfigBuilder,
        start: Start,
        settlements: mpsc::UnboundedReceiver<Dispatch>,
    ) -> Self {
        let (config, resuming) = match start {
            Start::Resume { session, address } => {
                info!(
                    sequence = session.sequence(),
                    "Discord gateway resumes the recorded session"
                );
                (config.session(session).resume_url(address.0), true)
            }
            Start::Identify(gap) => {
                warn!(reason = %gap, "Discord gateway starts a new session; messages sent while the provider was away are not replayed");
                (config, false)
            }
        };
        Self {
            shard: Shard::with_config(ShardId::ONE, config.build()),
            session: 0,
            unsettled: BTreeSet::new(),
            overflowed: false,
            settlements,
            resuming,
        }
    }

    /// The next message, interaction or gateway close, naming the dispatch
    /// of a message or interaction. Session facts are logged as they come.
    pub(crate) async fn next_event(
        &mut self,
    ) -> Option<Result<(Event, Option<Dispatch>), ReceiveMessageError>> {
        loop {
            let event = match self.shard.next_event(EVENTS).await? {
                Ok(event) => event,
                Err(error) => return Some(Err(error)),
            };
            if self.observe(&event) {
                continue;
            }
            return Some(Ok((event, self.dispatch())));
        }
    }

    /// The dispatch the shard received last.
    fn dispatch(&self) -> Option<Dispatch> {
        self.shard.session().map(|session| Dispatch {
            session: self.session,
            sequence: session.sequence(),
        })
    }

    /// Logs a session fact, returning whether `event` was only that. A new
    /// session leaves the unsettled messages of the old one behind.
    fn observe(&mut self, event: &Event) -> bool {
        match event {
            Event::Ready(_) => {
                self.session += 1;
                self.unsettled.clear();
                self.overflowed = false;
                if std::mem::take(&mut self.resuming) {
                    warn!(
                        "Discord did not resume the recorded gateway session and started a new one; messages sent while the provider was away are not replayed"
                    );
                } else {
                    info!("Discord gateway session started");
                }
                true
            }
            Event::Resumed => {
                self.resuming = false;
                info!("Discord resumed the gateway session; what was missed has been replayed");
                true
            }
            Event::GatewayInvalidateSession(resumable) => {
                warn!(resumable, "Discord invalidated the gateway session");
                true
            }
            _ => false,
        }
    }

    /// A message the provider took, unsettled until reported.
    pub(crate) fn taken(&mut self, dispatch: Option<Dispatch>) {
        if let Some(dispatch) = dispatch {
            self.unsettle(dispatch);
        }
        self.drain();
    }

    /// Keeps the floor below `dispatch` if it is of the current session.
    /// Past [`UNSETTLED_LIMIT`] this session cannot be recorded, so nothing
    /// more of it is kept.
    fn unsettle(&mut self, dispatch: Dispatch) {
        if dispatch.session != self.session || self.overflowed {
            return;
        }
        self.unsettled.insert(dispatch.sequence);
        if self.unsettled.len() > UNSETTLED_LIMIT {
            self.overflowed = true;
            self.unsettled.clear();
        }
    }

    /// Takes the settlements reported so far. One of an earlier session
    /// says nothing of the current one.
    fn drain(&mut self) {
        while let Ok(dispatch) = self.settlements.try_recv() {
            if dispatch.session == self.session {
                self.unsettled.remove(&dispatch.sequence);
            }
        }
    }

    /// Asks Discord to keep the session resumable and closes the connection,
    /// waiting at most [`CLOSE_BOUND`] for Discord's close. A message or
    /// interaction that arrives meanwhile is not taken, and stays unsettled.
    /// Returns whether Discord closed the connection.
    pub(crate) async fn close(&mut self) -> bool {
        self.shard.close(CloseFrame::RESUME);
        let closed = timeout(CLOSE_BOUND, async {
            while let Some(event) = self.shard.next_event(EVENTS).await {
                match event {
                    Ok(Event::GatewayClose(_)) => return true,
                    Ok(Event::MessageCreate(_) | Event::InteractionCreate(_)) => {
                        if let Some(dispatch) = self.dispatch() {
                            self.unsettle(dispatch);
                        }
                    }
                    Ok(event) => {
                        self.observe(&event);
                    }
                    Err(error) => {
                        warn!(%error, "Discord gateway failed while closing");
                        return false;
                    }
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        if closed {
            info!("Discord gateway closed for a resume");
        } else {
            warn!("Discord gateway did not confirm its close in time");
        }
        closed
    }

    /// The sequence a resume replays from: every dispatch up to it is
    /// settled. `None` when no session is left to resume.
    fn floor(&mut self) -> Option<(Session, String)> {
        self.drain();
        let session = self.shard.session()?;
        let address = self.shard.resume_url()?;
        let floor = match self.unsettled.first() {
            Some(earliest) => earliest.saturating_sub(1),
            None => session.sequence(),
        };
        Some((
            Session::new(floor, session.id().to_owned()),
            address.to_owned(),
        ))
    }

    /// Records the session for the next start, once every message this
    /// process took has settled or never will: after the provider's work has
    /// ended. Logs what was recorded, or why nothing was.
    pub(crate) fn record(&mut self, path: &Path, identity: &Identity) {
        if self.overflowed {
            warn!(
                limit = UNSETTLED_LIMIT,
                "more Discord messages went unsettled than a session record covers; the next start begins a new session"
            );
            return;
        }
        let Some((session, resume_url)) = self.floor() else {
            warn!("no Discord gateway session is left to resume; the next start begins a new one");
            return;
        };
        if !session_id_valid(session.id()) || ResumeAddress::parse(&resume_url).is_none() {
            warn!("Discord's gateway session cannot be recorded; the next start begins a new one");
            return;
        }
        let record = Record {
            identity: identity.clone(),
            session_id: session.id().to_owned(),
            sequence: session.sequence(),
            resume_url,
            recorded_at: now(),
        };
        let bytes = match serde_json::to_vec(&record) {
            Ok(bytes) => bytes,
            Err(error) => {
                warn!(%error, "the Discord gateway session could not be recorded");
                return;
            }
        };
        match publish(path, &bytes, 0o600) {
            Ok(()) => info!(
                sequence = session.sequence(),
                "Discord gateway session recorded for the next start"
            ),
            Err(PublishError::Unsynced(error)) => warn!(
                %error,
                "the Discord gateway session was recorded, but a system crash may lose the record"
            ),
            Err(error) => warn!(
                %error,
                "the Discord gateway session could not be recorded; the next start begins a new one"
            ),
        }
    }
}

/// Where the provider keeps its session record.
pub(crate) fn record_path(state: &Path) -> PathBuf {
    state.join("gateway-session.json")
}

#[cfg(test)]
pub(crate) mod tests;
