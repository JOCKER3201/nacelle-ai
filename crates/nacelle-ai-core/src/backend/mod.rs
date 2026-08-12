//! The contract every provider implements, and the only thing the agent
//! loop knows about who is answering.
//!
//! [`Backend::send`] blocks for the whole turn and reports progress
//! through a callback rather than returning an iterator. Three reasons,
//! in order of how much they cost to get wrong:
//!
//! * An iterator would have to own the open HTTP response *and* hand out
//!   borrowed items, which in Rust means either a self-referential type
//!   or an allocation per event. A callback is neither.
//! * The receiver can stop the turn by returning [`Flow::Stop`], so
//!   cancelling is the same code path on every backend instead of a
//!   per-provider afterthought.
//! * The callback runs on the worker thread, so the natural
//!   implementation — push each event into an [`std::sync::mpsc::Sender`]
//!   the desktop's event loop drains — needs no adapter.
//!
//! A backend is `Send` because it lives on that worker thread. It is not
//! `Sync`: one turn at a time, one thread at a time. Two concurrent
//! turns are two backends.
//!
//! One provider per submodule.

pub mod anthropic;
pub mod ollama;

use crate::error::BackendError;
use crate::event::StreamEvent;
use crate::message::Request;

/// What the receiver wants after seeing an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// Keep the turn running.
    Continue,
    /// Abandon the turn. The backend stops reading, drops the
    /// connection and returns [`BackendError::Cancelled`]; it does not
    /// emit [`StreamEvent::End`], because the turn did not end — it was
    /// abandoned, and a caller must be able to tell those apart.
    Stop,
}

/// Where a backend puts events as they arrive.
///
/// Written as a trait object so [`Backend`] stays object-safe: the agent
/// loop holds a `Box<dyn Backend>` chosen at runtime from configuration.
pub type EventSink<'a> = dyn FnMut(StreamEvent) -> Flow + 'a;

/// One provider.
///
/// Implementors are responsible for everything the provider does that
/// [`StreamEvent`] does not describe: request encoding, authentication
/// headers, SSE or NDJSON framing, reassembling chunked tool arguments,
/// and mapping status codes onto [`BackendError`].
pub trait Backend: Send {
    /// A short, stable name for logs and for the user's configuration —
    /// `"anthropic"`, `"ollama"`. Never a URL, and never anything
    /// derived from a credential.
    fn name(&self) -> &str;

    /// Run one turn, blocking until it finishes.
    ///
    /// On success the sink has been called with exactly one
    /// [`StreamEvent::Start`] first and exactly one
    /// [`StreamEvent::End`] last. On failure it may have been called any
    /// number of times, but never with `End`: a turn that produced text
    /// and then broke is a failure, and the error is the only truth
    /// about it.
    fn send(
        &mut self,
        request: &Request,
        sink: &mut EventSink<'_>,
    ) -> Result<(), BackendError>;
}
