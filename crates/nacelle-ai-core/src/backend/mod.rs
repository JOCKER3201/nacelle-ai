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
//! One provider per submodule, and one rule that spans them: **a
//! provider this program authenticates to is a provider whose requests
//! go through [`supervise::seal`](crate::supervise::seal) first.** The
//! rule is not enforced by this trait — a trait cannot demand that an
//! implementor's private encoder take a particular argument — so it is
//! enforced where it can be, inside the one backend it applies to:
//! [`anthropic::Anthropic`] holds a [`Seal`](crate::supervise::Seal) it
//! cannot be built without, and its encoder takes a
//! [`Sealed`](crate::supervise::Sealed) request and nothing else. A
//! future third-party backend is written the same way, and the reason
//! it must be is in that module's header.
//!
//! [`ollama::Ollama`] deliberately has no seal: the layers exist for
//! bytes reaching a third party under a credential, and it reaches
//! neither.

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

    /// Whether a turn on this backend stays on the user's own machine.
    ///
    /// The default is `false`, which is the safe answer: a backend that
    /// has not thought about the question is one whose bytes might
    /// leave. Two things ask, and both would be defeated by a hopeful
    /// default — the escalation policy, which is the difference between
    /// answering locally and sending, and
    /// [`LocalReviewer`](crate::redact::LocalReviewer), which refuses to
    /// hand a payload to a remote model in order to ask whether the
    /// payload may be sent.
    fn is_local(&self) -> bool {
        false
    }

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

    /// Tell this backend how to find out whether the turn it is about to
    /// run has been stopped.
    ///
    /// Defaulted to nothing, because a local backend has nothing to stop
    /// early: its bytes do not leave. What it is for is the window
    /// between "the caller asked for a turn" and "the request was
    /// posted", which on a remote backend is where the seal's layers
    /// run — layer 3 is a whole turn against a local model and layer 4
    /// waits on a person. [`Flow::Stop`] cannot cover that window: the
    /// sink is not called until the reply is being decoded, which is
    /// after the socket.
    fn stops_when(&mut self, _stop: crate::supervise::seal::Stop) {}
}
