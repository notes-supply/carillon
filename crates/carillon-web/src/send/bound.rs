//! A bound on what a multipart body may make the provider hold between its
//! parts, and read once it has refused the message.
//!
//! A part's data is bounded as it is staged, and the whole request is not,
//! because a file may be large. Nothing bounds the bytes a peer sends before
//! the first boundary, or in a part's headers, which the parser buffers whole
//! while it looks for their end. A header that never ends makes the provider
//! hold all of it.
//!
//! [`Meter::feed`] feeds the parser the body at most [`BETWEEN_PARTS`] bytes
//! between one part and the next. [`Meter::parts`] opens that window before
//! a boundary and its headers are read; [`Meter::data`] lifts it while a
//! part's data streams, since that data is bounded elsewhere.
//! [`Meter::refuse`] caps every later window at [`REFUSED_DRAIN`] past the
//! refusal, so the rest of a refused message is read within one budget,
//! boundaries and headers included. A window a peer overruns ends the request
//! with [`BodyError::Overrun`], which the handler answers 413.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering::Relaxed},
};

use axum::body::Body;
use bytes::Bytes;
use futures_util::{Stream, StreamExt as _, stream};

/// The most a peer may send between two parts: the preamble before the first
/// boundary, a boundary line, and a part's headers. A real boundary and
/// headers are far smaller; this is room, not a target.
pub(crate) const BETWEEN_PARTS: usize = 64 * 1024;

/// The most the provider reads of a message after refusing it, so a browser
/// still reads the refusal where it can, without a peer draining the provider
/// with a refused upload. A refused message is normally small; only an
/// oversized text or file leaves more than this, and that is the message
/// being refused.
pub(crate) const REFUSED_DRAIN: usize = 2 * 1024 * 1024;

/// Why the body stopped feeding the parser.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BodyError {
    /// The parser asked past the open window: a preamble or part headers
    /// past [`BETWEEN_PARTS`], or a refused message past [`REFUSED_DRAIN`].
    #[error("a multipart body is larger than the provider reads")]
    Overrun,
    #[error(transparent)]
    Read(axum::Error),
}

/// How far into the body the parser may read. The handler opens the
/// windows, and the body it feeds the parser spends them.
pub(crate) struct Meter {
    /// Bytes the body has fed the parser.
    fed: AtomicUsize,
    /// Where the open window ends.
    end: AtomicUsize,
    /// Where every window ends once the message is refused.
    limit: AtomicUsize,
}

impl Meter {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            fed: AtomicUsize::new(0),
            end: AtomicUsize::new(0),
            limit: AtomicUsize::new(usize::MAX),
        })
    }

    /// `body` as the parser reads it: no further than the open window, and
    /// every byte counted. A frame that outruns the window is fed in parts.
    ///
    /// When the window is spent, the body waits once, so the parser parses
    /// what it holds. A parser that asks again with the window still spent
    /// has asked past it, and the body ends with [`BodyError::Overrun`].
    pub(crate) fn feed(
        self: &Arc<Self>,
        body: Body,
    ) -> impl Stream<Item = Result<Bytes, BodyError>> + Send + 'static {
        let start = (Arc::clone(self), body.into_data_stream(), Bytes::new());
        stream::try_unfold(start, |(meter, mut body, mut held)| async move {
            if meter.budget() == 0 {
                tokio::task::yield_now().await;
                if meter.budget() == 0 {
                    return Err(BodyError::Overrun);
                }
            }
            if held.is_empty() {
                let Some(frame) = body.next().await else {
                    return Ok(None);
                };
                held = frame.map_err(BodyError::Read)?;
            }
            let fed = held.split_to(held.len().min(meter.budget()));
            meter.fed.fetch_add(fed.len(), Relaxed);
            Ok(Some((fed, (meter, body, held))))
        })
    }

    /// Bounds the next boundary and part headers to [`BETWEEN_PARTS`] bytes.
    pub(crate) fn parts(&self) {
        self.open(self.fed.load(Relaxed).saturating_add(BETWEEN_PARTS));
    }

    /// Lets a part's own data stream without this bound, which the stager and
    /// the event's size limit hold instead.
    pub(crate) fn data(&self) {
        self.open(usize::MAX);
    }

    /// Reads the rest of a refused message no further than [`REFUSED_DRAIN`]
    /// past this point. Only the first refusal sets the limit.
    pub(crate) fn refuse(&self) {
        let limit = self.fed.load(Relaxed).saturating_add(REFUSED_DRAIN);
        let limit = self.limit.fetch_min(limit, Relaxed).min(limit);
        self.end.fetch_min(limit, Relaxed);
    }

    fn open(&self, end: usize) {
        self.end.store(end.min(self.limit.load(Relaxed)), Relaxed);
    }

    fn budget(&self) -> usize {
        self.end
            .load(Relaxed)
            .saturating_sub(self.fed.load(Relaxed))
    }
}
