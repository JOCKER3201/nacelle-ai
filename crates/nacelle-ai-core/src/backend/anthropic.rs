//! Anthropic's Messages API, spoken directly over HTTP.
//!
//! There is no official Anthropic SDK for Rust, and raw HTTP is the
//! documented route for languages without one. That is not a hardship
//! here: the endpoint is one POST, the reply is one SSE stream, and the
//! work is in the details rather than the plumbing.
//!
//! The pieces, in the order a turn passes through them:
//!
//! | module | what it does |
//! |---|---|
//! | [`body`] | the conversation, encoded the way the endpoint wants it |
//! | [`transport`] | the only place a socket is opened |
//! | [`sse`] | which bytes belong to which stream event |
//! | [`decode`] | stream events, turned into the contract's [`StreamEvent`]s |
//!
//! What is worth knowing before changing any of it:
//!
//! * **Nothing is encoded that has not been through the layers.**
//!   [`body::build`] takes a [`Sealed`] request and there is no other
//!   way to produce a body here; [`Seal::seal`] is the only way to
//!   produce a `Sealed`, and it runs layers 2, 3 and 4 every time. This
//!   is the one place in the program where the user's text meets a
//!   socket, so it is the one place worth making impossible to walk
//!   past rather than merely inadvisable to. See
//!   [`supervise::seal`](crate::supervise::seal).
//! * **The credential decides a header name, not just a value.** An API
//!   key goes in `x-api-key`; an OAuth token goes in `authorization` and
//!   needs a beta flag alongside it. Sending an OAuth token as
//!   `x-api-key` is a 401 that says nothing useful, which is why the
//!   choice is made once, in
//!   [`Credential::auth_headers`](crate::credentials::Credential::auth_headers),
//!   and this backend only merges beta flags into it.
//! * **Thinking has to be asked for, and cannot be budgeted.** See
//!   [`body`].
//! * **A refusal is an error.** The provider reports it as HTTP 200 with
//!   `stop_reason: "refusal"`. See [`decode`].
//! * **Only a turn that emitted nothing is retried.** Once the receiver
//!   has seen a `Start`, asking again would send it a second one.

mod body;
mod decode;
mod sse;
mod transport;

use std::io::Read;
use std::time::Duration;

use serde_json::Value;

use crate::backend::{Backend, EventSink};
use crate::credentials::{Credential, HEADER_ANTHROPIC_BETA};
use crate::error::BackendError;
use crate::event::StreamEvent;
use crate::message::Request;
use crate::supervise::seal::{Seal, Sealed};

pub use transport::{HttpResponse, HttpTransport, Transport};

/// The endpoint. One POST does everything this backend does.
pub const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";

/// The API version header. It pins the wire format, not the model, and
/// it is required on every request.
pub const API_VERSION: &str = "2023-06-01";

const HEADER_CONTENT_TYPE: &str = "content-type";
const HEADER_API_VERSION: &str = "anthropic-version";

/// The name this backend answers to. Never a URL, never anything derived
/// from the credential.
pub const NAME: &str = "anthropic";

/// The provider's word for "I ran out of room in the middle of the job".
///
/// It arrives as [`StopReason::Other`](crate::event::StopReason::Other)
/// because it is not an ending: the model paused mid-task and the work is
/// unfinished. Send the conversation back unchanged — including the
/// assistant turn that just arrived — and the model carries on from where
/// it stopped.
pub const STOP_PAUSE_TURN: &str = "pause_turn";

/// What to set [`Request::max_tokens`] to for a real turn against these
/// models.
///
/// The contract's default is deliberately modest, and it predates this
/// backend. These models think inside the output budget and can spend a
/// lot of it before the first visible word, so a tight ceiling shows up
/// as an answer that stops mid-sentence with
/// [`StopReason::MaxTokens`](crate::event::StopReason::MaxTokens).
/// Streaming is what makes a budget this large safe to ask for, and this
/// backend always streams.
pub const RECOMMENDED_MAX_TOKENS: u32 = 64_000;

/// One model the user may pick.
///
/// The ids are complete as written. Nothing appends a date suffix to
/// them: a dated variant is a different model identifier, and inventing
/// one produces a 404 rather than a newer model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Model {
    /// Exactly what goes into [`Request::model`].
    pub id: &'static str,
    /// One line, for a menu. Which model to use is the user's decision,
    /// so the job here is to describe rather than to recommend.
    pub summary: &'static str,
}

/// The most capable of the three: long autonomous work, hard reasoning,
/// agentic tool use. What this agent defaults to.
pub const OPUS_4_8: Model = Model {
    id: "claude-opus-4-8",
    summary: "Most capable. Long autonomous work, hard reasoning, heavy tool use.",
};

/// Close to Opus on most work and cheaper, which makes it the one to
/// reach for when a turn happens often.
pub const SONNET_5: Model = Model {
    id: "claude-sonnet-5",
    summary: "Near-Opus quality at lower cost. The everyday choice for frequent turns.",
};

/// The quick one. Good for short, well-defined turns where waiting is
/// the thing the user would notice.
pub const HAIKU_4_5: Model = Model {
    id: "claude-haiku-4-5",
    summary: "Fastest and cheapest. Short, simple, latency-sensitive turns.",
};

/// Every model this backend offers, in the order a menu should show them.
pub const MODELS: [Model; 3] = [OPUS_4_8, SONNET_5, HAIKU_4_5];

/// What [`Request::model`] should be set to when the user has not chosen.
pub const DEFAULT_MODEL: Model = OPUS_4_8;

/// How much thinking and work to spend on a turn: `output_config.effort`.
///
/// This replaced the old fixed thinking budget, and it governs more than
/// thinking — at lower settings the model also takes fewer, more
/// consolidated tool calls and writes less around its answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Effort {
    /// Short, scoped turns where latency is what the user notices.
    Low,
    /// Cheaper than the default, at some cost in thoroughness.
    Medium,
    /// The default, and the sensible floor for anything that matters.
    #[default]
    High,
    /// The best setting for coding and agentic work.
    XHigh,
    /// Correctness over cost. Can overthink; worth measuring before
    /// leaving it on.
    Max,
}

impl Effort {
    /// The wire value. `xhigh` has no separator, which is easy to get
    /// wrong and rejected when you do.
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

/// How hard to try again when a turn fails for a reason that might not
/// recur.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retry {
    /// Total attempts, the first one included. `1` never retries.
    pub attempts: u32,
    /// The first wait. Each further wait doubles it.
    pub backoff: Duration,
    /// The longest any single wait may be — including one the provider
    /// asked for. A provider that says "come back in an hour" is not
    /// allowed to park a worker thread for an hour; the caller decides
    /// what to do about a wait that long.
    pub cap: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Retry {
            attempts: 3,
            backoff: Duration::from_millis(500),
            cap: Duration::from_secs(30),
        }
    }
}

/// The Anthropic backend.
///
/// Holds a credential, a transport and the [`Seal`], and nothing else
/// that a turn depends on: the model, the tools and the conversation all
/// arrive with the [`Request`], because they change between turns and
/// these do not.
///
/// The seal is a field rather than an argument because it remembers
/// across turns — what the user has already been shown, and what the
/// last failure said about whether the provider is reachable at all —
/// and because a backend that could be built without one would be a
/// backend that could send unredacted text.
pub struct Anthropic<T = HttpTransport> {
    credential: Credential,
    transport: T,
    seal: Seal,
    endpoint: String,
    effort: Effort,
    summarise_thinking: bool,
    betas: Vec<String>,
    retry: Retry,
}

impl Anthropic<HttpTransport> {
    /// A backend that talks to the real endpoint.
    pub fn new(credential: Credential, seal: Seal) -> Self {
        Anthropic::with_transport(credential, seal, HttpTransport::new())
    }
}

impl<T: Transport> Anthropic<T> {
    /// A backend that talks through `transport`.
    ///
    /// This is how the protocol is tested without a network, and how a
    /// desktop will later route requests through a proxy the user
    /// configured.
    pub fn with_transport(credential: Credential, seal: Seal, transport: T) -> Self {
        Anthropic {
            credential,
            transport,
            seal,
            endpoint: ENDPOINT.to_string(),
            effort: Effort::default(),
            // On by default: the agent shows the user what it is doing,
            // and without this the thinking blocks arrive empty.
            summarise_thinking: true,
            betas: Vec::new(),
            retry: Retry::default(),
        }
    }

    pub fn with_effort(mut self, effort: Effort) -> Self {
        self.effort = effort;
        self
    }

    /// Ask for readable summaries of the model's reasoning, rather than
    /// the empty thinking blocks the endpoint sends by default. Only
    /// matters when the request asked for thinking at all.
    pub fn with_thinking_summary(mut self, summarise: bool) -> Self {
        self.summarise_thinking = summarise;
        self
    }

    /// Add a beta flag.
    ///
    /// Flags are merged, comma-separated, into the single
    /// `anthropic-beta` header — including the one an OAuth credential
    /// brings with it. A second header of that name would be ignored:
    /// the endpoint reads one.
    pub fn with_beta(mut self, flag: impl Into<String>) -> Self {
        self.betas.push(flag.into());
        self
    }

    /// Point the backend at something other than the public endpoint — a
    /// gateway, or a proxy inside a network. It must not carry a
    /// credential in the URL: this string appears in error messages.
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    pub fn with_retry(mut self, retry: Retry) -> Self {
        self.retry = retry;
        self
    }

    /// The seal, for an interface that wants to say whether this
    /// session may reach the provider at all, or to pin it so that it
    /// may not.
    ///
    /// Handing it out is safe in a way that handing out an encoder
    /// would not be: everything on a [`Seal`] either reports the
    /// policy or narrows it, and the one method that produces
    /// something sendable is the one that runs the layers.
    pub fn seal(&mut self) -> &mut Seal {
        &mut self.seal
    }

    /// Every header the request needs, and nothing derived from anything
    /// else.
    fn headers(&self) -> Vec<(&'static str, String)> {
        let mut headers = vec![
            (HEADER_CONTENT_TYPE, "application/json".to_string()),
            (HEADER_API_VERSION, API_VERSION.to_string()),
        ];

        // The credential decides which header it goes in — that decision
        // is made once, where the credential is, and never here. What is
        // this backend's business is that an OAuth credential arrives
        // with a beta flag, and beta flags have to end up in one header
        // rather than several.
        let mut betas: Vec<String> = Vec::new();
        for (name, value) in self.credential.auth_headers() {
            if name == HEADER_ANTHROPIC_BETA {
                betas.push(value);
            } else {
                headers.push((name, value));
            }
        }
        betas.extend(self.betas.iter().cloned());

        if !betas.is_empty() {
            headers.push((HEADER_ANTHROPIC_BETA, betas.join(",")));
        }

        headers
    }

    /// The bytes of one turn.
    ///
    /// Takes the [`Sealed`] request rather than the [`Request`] the
    /// caller handed over, and so does everything below it. That is the
    /// whole mechanism: there is no path from a `Request` to a socket in
    /// this file that does not pass through [`Seal::seal`].
    fn encode(&self, sealed: &Sealed) -> Result<Vec<u8>, BackendError> {
        serde_json::to_vec(&body::build(sealed, self.effort, self.summarise_thinking)).map_err(
            |err| BackendError::Protocol(format!("the request could not be encoded: {err}")),
        )
    }

    /// One attempt: send, then either read the stream or work out what
    /// the status meant.
    fn attempt(
        &self,
        headers: &[(&'static str, String)],
        body: &[u8],
        model: &str,
        sink: &mut EventSink<'_>,
    ) -> Result<(), BackendError> {
        let response = self.transport.post(&self.endpoint, headers, body)?;
        if response.status == 200 {
            decode::run(response.body, model, sink)
        } else {
            Err(status_error(response))
        }
    }
}

impl<T: Transport> Backend for Anthropic<T> {
    fn name(&self) -> &str {
        NAME
    }

    /// Handed straight to the seal, which is what is actually in the
    /// window: everything between the caller asking for a turn and
    /// `transport.post` is layers 2, 3 and 4.
    fn stops_when(&mut self, stop: crate::supervise::seal::Stop) {
        self.seal.stops_when(stop);
    }

    fn send(&mut self, request: &Request, sink: &mut EventSink<'_>) -> Result<(), BackendError> {
        // Layers 2, 3 and 4, before anything else happens and with
        // nothing to skip them with. A refusal here — a pinned session,
        // a provider that has stopped answering, a user who read the
        // manifest and said no — is not a failed turn: no socket was
        // opened, and the sentence that comes back says which of those
        // it was.
        let sealed = self.seal.seal(request)?;
        let body = self.encode(&sealed)?;
        let headers = self.headers();

        let attempts = self.retry.attempts.max(1);
        let mut attempt = 1;
        let mut wait = self.retry.backoff;

        loop {
            // Whether this attempt reached the receiver at all. Watched
            // rather than assumed, because it is what decides if trying
            // again is even allowed.
            let mut emitted = false;
            let outcome = {
                let mut watch = |event: StreamEvent| {
                    emitted = true;
                    sink(event)
                };
                self.attempt(&headers, &body, &sealed.request().model, &mut watch)
            };

            let err = match outcome {
                Ok(()) => return Ok(()),
                Err(err) => err,
            };

            // A turn that already produced events cannot be retried: the
            // receiver has seen a Start and some of an answer, and a
            // second attempt would send it a second Start and repeat the
            // text. Exactly one Start per turn is the contract, so a
            // failure after the first event is final — the caller can
            // start a new turn if it wants one.
            if emitted || attempt >= attempts || !err.is_retryable() {
                // What this turn learned about the remote half, before
                // the error leaves. A provider that could not be
                // reached, or a credential it rejected, stops the NEXT
                // turn at the seal instead of at a socket — which is
                // the point of "no network degrades exactly like a
                // pin": the local half never waits on the remote half
                // twice to find out the same thing.
                self.seal.observe(&err);
                return Err(err);
            }

            // The provider's own figure wins when it gave one, since it
            // knows when the limit resets — but never past the cap.
            let delay = err.retry_after().unwrap_or(wait).min(self.retry.cap);
            std::thread::sleep(delay);
            // Checked, because doubling a `Duration` past its ceiling
            // panics — and a backoff schedule is the last place worth
            // taking a process down over.
            wait = wait
                .checked_mul(2)
                .unwrap_or(self.retry.cap)
                .min(self.retry.cap);
            attempt += 1;
        }
    }
}

/// How much of an error body to read. They are short; this is a bound on
/// a body that has already gone wrong, not a size anyone expects to hit.
const MAX_ERROR_BYTES: u64 = 16 * 1024;

/// How much of it to repeat back when it turns out not to be JSON.
const MAX_ERROR_CHARS: usize = 400;

/// What a non-200 meant.
///
/// The line that matters is whether asking again could work:
///
/// * 401 and 403 — the credential was rejected. Another attempt with the
///   same one is pointless; the user has to supply a different token.
/// * 429 — rate limited, and the header says when to come back.
/// * 5xx, 529 included — the provider's own trouble. Worth retrying.
/// * everything else, 400 and 404 and 413 among them — the request was
///   wrong. It will be just as wrong the second time, so it is reported
///   with whatever the provider said was wrong with it.
fn status_error(response: HttpResponse) -> BackendError {
    let HttpResponse {
        status,
        retry_after,
        body,
    } = response;

    let mut text = String::new();
    // Best effort: a truncated or non-UTF-8 error body still leaves the
    // status code, which is the part that decides what happens next.
    let _ = body.take(MAX_ERROR_BYTES).read_to_string(&mut text);
    let message = explain(&text).unwrap_or_else(|| format!("HTTP {status}, no detail given"));

    match status {
        401 | 403 => BackendError::Auth(message),
        429 => BackendError::RateLimited {
            retry_after,
            message,
        },
        _ => BackendError::Server { status, message },
    }
}

/// The provider's own account of what was wrong, out of the error body.
///
/// Built only from the body and the status. Never from a request header:
/// an error is the most likely thing to be logged, mailed or pasted into
/// a bug report, and the credential is in a header.
fn explain(body: &str) -> Option<String> {
    let body = body.trim();
    if body.is_empty() {
        return None;
    }

    if let Ok(value) = serde_json::from_str::<Value>(body) {
        let error = value.get("error");
        let kind = error
            .and_then(|error| error.get("type"))
            .and_then(Value::as_str);
        let message = error
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str);

        match (kind, message) {
            (Some(kind), Some(message)) => return Some(format!("{kind}: {message}")),
            (None, Some(message)) => return Some(message.to_string()),
            (Some(kind), None) => return Some(kind.to_string()),
            (None, None) => {}
        }
    }

    // Not the documented shape — a gateway or a proxy answered instead.
    // Pass on a bounded amount of it verbatim, because whatever it says
    // is the only clue there is.
    let cut = body
        .char_indices()
        .nth(MAX_ERROR_CHARS)
        .map(|(at, _)| at)
        .unwrap_or(body.len());
    Some(body[..cut].to_string())
}
