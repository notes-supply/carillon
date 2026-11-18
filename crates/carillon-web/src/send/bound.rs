//! A bound on what a multipart body may make the provider hold between its
//! parts.
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
//! a part's data streams, since that data is bounded elsewhere. A window a
//! peer overruns ends the request with [`PartHeaderTooLarge`], which the
//! handler answers 413.

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

/// A window of [`BETWEEN_PARTS`] bytes was overrun, so the request is refused
/// rather than held.
#[derive(Debug)]
pub(crate) struct PartHeaderTooLarge;

impl std::fmt::Display for PartHeaderTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a multipart part's headers are larger than the provider reads")
    }
}

impl std::error::Error for PartHeaderTooLarge {}

/// How many bytes the body may still feed the parser before the current
/// window is reset, and whether the parser was already told the window was
/// spent. The handler and the body share one.
pub(crate) struct Meter {
    budget: AtomicUsize,
    spent: AtomicBool,
}

impl Meter {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            budget: AtomicUsize::new(0),
            spent: AtomicBool::new(false),
        })
    }

    /// Bounds the next boundary and part headers to [`BETWEEN_PARTS`] bytes.
    pub(crate) fn parts(&self) -> &Self {
        self.open(BETWEEN_PARTS);
        self
    }

    /// Lets a part's own data stream without this bound, which the stager and
    /// the event's size limit hold instead.
    pub(crate) fn data(&self) {
        self.open(usize::MAX);
    }

    fn open(&self, budget: usize) {
        self.budget.store(budget, Relaxed);
        self.spent.store(false, Relaxed);
    }
}

/// The body, feeding the parser no more than the open window allows. When the
/// window is `usize::MAX` the body flows untouched.
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
        if budget == usize::MAX {
            return chunk;
        }
        if chunk.len() <= budget {
            self.meter.budget.fetch_sub(chunk.len(), Relaxed);
            return chunk;
        }
        let head = chunk.split_to(budget);
        self.leftover = Some(chunk);
        self.meter.budget.store(0, Relaxed);
        head
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

impl Stream for Metered {
    type Item = Result<Bytes, BoxError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let budget = this.meter.budget.load(Relaxed);
        if budget == 0 {
            // The window is spent. The parser is told once to work with what
            // it has; a second ask with no part read in between means the
            // headers or preamble outran the window, so the request is
            // refused. The parser only asks again once more body has arrived,
            // which a self-wake lets it do while the body stays parked.
            if this.meter.spent.swap(true, Relaxed) {
                return Poll::Ready(Some(Err(Box::new(PartHeaderTooLarge))));
            }
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        if let Some(chunk) = this.leftover.take() {
            return Poll::Ready(Some(Ok(this.hand_over(chunk, budget))));
        }
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => Poll::Ready(Some(Ok(this.hand_over(chunk, budget)))),
            Poll::Ready(Some(Err(error))) => Poll::Ready(Some(Err(error.into()))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}
