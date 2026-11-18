//! A bound on what a multipart body may make the provider hold between its
//! parts, and read once it has refused the message.
//!
//! A part's data is bounded as it is staged, and the whole request is not,
//! because a file may be large. Nothing bounds the bytes a peer sends before
//! the first boundary, or in a part's headers, which the parser buffers whole
//! while it looks for their end. A header that never ends makes the provider
//! hold all of it.
//!
//! [`Metered`] wraps the body and feeds the parser at most [`BETWEEN_PARTS`]
//! bytes between one part and the next. [`Meter::parts`] opens that window
//! before a boundary and its headers are read; [`Meter::data`] lifts it while
//! a part's data streams, since that data is bounded elsewhere.
//! [`Meter::refuse`] caps every later window at [`REFUSED_DRAIN`] past the
//! refusal, so the rest of a refused message is read within one budget,
//! boundaries and headers included. A window a peer overruns ends the request
//! with [`BodyError::Overrun`], which the handler answers 413.

use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
    },
    task::{Context, Poll},
};

use axum::body::{Body, BodyDataStream};
use bytes::Bytes;
use futures_util::Stream;

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

/// How far into the body the parser may read, and whether it was already
/// told the open window was spent. The handler and the body share one.
pub(crate) struct Meter {
    /// Bytes the body has fed the parser.
    fed: AtomicUsize,
    /// Where the open window ends.
    end: AtomicUsize,
    /// Where every window ends once the message is refused.
    limit: AtomicUsize,
    spent: AtomicBool,
}

impl Meter {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            fed: AtomicUsize::new(0),
            end: AtomicUsize::new(0),
            limit: AtomicUsize::new(usize::MAX),
            spent: AtomicBool::new(false),
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
        self.spent.store(false, Relaxed);
    }

    fn budget(&self) -> usize {
        self.end
            .load(Relaxed)
            .saturating_sub(self.fed.load(Relaxed))
    }
}

/// The body, feeding the parser no more than the open window allows.
pub(crate) struct Metered {
    inner: BodyDataStream,
    meter: Arc<Meter>,
    /// Bytes split off a frame that overran the window, held until the window
    /// reopens.
    leftover: Option<Bytes>,
}

impl Metered {
    pub(crate) fn new(body: Body, meter: Arc<Meter>) -> Self {
        Self {
            inner: body.into_data_stream(),
            meter,
            leftover: None,
        }
    }

    /// Yields up to `budget` bytes of `chunk`, holding any remainder. The
    /// parser gets the window's worth; the rest waits for the window to
    /// reopen, when the part it belongs to is read.
    fn hand_over(&mut self, mut chunk: Bytes, budget: usize) -> Bytes {
        if chunk.len() > budget {
            self.leftover = Some(chunk.split_off(budget));
        }
        self.meter.fed.fetch_add(chunk.len(), Relaxed);
        chunk
    }
}

impl Stream for Metered {
    type Item = Result<Bytes, BodyError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let budget = this.meter.budget();
        if budget == 0 {
            // The window is spent. The parser is told once to work with what
            // it has; a second ask with no part read in between means the
            // headers, the preamble or a refused message outran the window,
            // so the request ends. The parser only asks again once more body
            // has arrived, which a self-wake lets it do while the body stays
            // parked.
            if this.meter.spent.swap(true, Relaxed) {
                return Poll::Ready(Some(Err(BodyError::Overrun)));
            }
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        if let Some(chunk) = this.leftover.take() {
            return Poll::Ready(Some(Ok(this.hand_over(chunk, budget))));
        }
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => Poll::Ready(Some(Ok(this.hand_over(chunk, budget)))),
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(BodyError::Read(error)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}
