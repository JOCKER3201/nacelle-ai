//! How a backend fails, in the few shapes a caller can actually do
//! something about.
//!
//! The distinctions here are the ones that change behaviour: retry now,
//! retry later, ask the user for a token, or give up. Provider-specific
//! detail that would not change the decision is flattened into text.
//!
//! Nothing in this module ever carries a credential. An error is the
//! most likely thing to be logged, mailed or pasted into a bug report,
//! so a backend must build its messages out of status codes and provider
//! text, never out of a request header.

use std::error::Error;
use std::fmt;
use std::time::Duration;

/// Why a turn did not finish.
#[derive(Clone, Debug, PartialEq)]
pub enum BackendError {
    /// The request never got an answer: DNS, TLS, connect, reset, a
    /// truncated response body. Worth retrying.
    Network(String),
    /// The provider rejected the credential (HTTP 401/403). Retrying with
    /// the same token is pointless — the user has to supply another one.
    Auth(String),
    /// Rate limited (HTTP 429). `retry_after` carries the provider's
    /// `Retry-After` when it sent one; `None` means it did not, and the
    /// caller should back off on its own schedule.
    RateLimited {
        retry_after: Option<Duration>,
        message: String,
    },
    /// The model declined the request. Not a transport failure: the
    /// request arrived, was understood, and was refused. `category` is
    /// the provider's own label when it gave one.
    Refused {
        category: Option<String>,
        explanation: Option<String>,
    },
    /// The provider answered with something this backend cannot read:
    /// a frame it does not know, tool arguments that are not JSON, a
    /// stream that stopped mid-object. A bug on one side or the other,
    /// not a condition to retry into.
    Protocol(String),
    /// The provider failed on its own account (HTTP 5xx, including the
    /// overloaded case). Worth retrying.
    Server { status: u16, message: String },
    /// The local agent did not let this leave the machine, so no socket
    /// was ever opened: the session is pinned local, there is no
    /// credential, the provider has stopped answering, or the user saw
    /// what would have been sent and said no. See
    /// [`supervise::seal`](crate::supervise::seal).
    ///
    /// The string is what the agent says out loud. It is written for
    /// the person in front of the machine and for the model that has to
    /// plan around it, which is why it is a sentence rather than a
    /// code: both of them need to know what could not be done here, not
    /// merely that something failed.
    Withheld(String),
    /// The receiver asked the stream to stop — see
    /// [`Flow::Stop`](crate::backend::Flow). Expected, not a fault; it is
    /// an error only because it means the turn has no result.
    Cancelled,
}

impl BackendError {
    /// Whether trying the same request again could plausibly work.
    ///
    /// A caller that retries on everything hammers a provider that has
    /// already said no; one that retries on nothing gives up on a
    /// dropped connection. This is the line between the two.
    pub fn is_retryable(&self) -> bool {
        match self {
            BackendError::Network(_) | BackendError::RateLimited { .. } => true,
            BackendError::Server { status, .. } => *status >= 500,
            BackendError::Auth(_)
            | BackendError::Refused { .. }
            | BackendError::Protocol(_)
            // Retrying a withheld request would put the same manifest
            // in front of the same user, or ask the same pinned policy
            // the same question. The answer does not change by being
            // asked twice, and asking twice is how a refusal gets
            // clicked through.
            | BackendError::Withheld(_)
            | BackendError::Cancelled => false,
        }
    }

    /// How long the provider asked us to wait, if it said.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            BackendError::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::Network(msg) => write!(f, "network error: {msg}"),
            BackendError::Auth(msg) => write!(f, "the provider rejected the credential: {msg}"),
            BackendError::RateLimited {
                retry_after,
                message,
            } => match retry_after {
                Some(wait) => write!(
                    f,
                    "rate limited, retry in {}s: {message}",
                    wait.as_secs().max(1)
                ),
                None => write!(f, "rate limited: {message}"),
            },
            BackendError::Refused {
                category,
                explanation,
            } => {
                write!(f, "the model refused the request")?;
                if let Some(category) = category {
                    write!(f, " ({category})")?;
                }
                if let Some(explanation) = explanation {
                    write!(f, ": {explanation}")?;
                }
                Ok(())
            }
            BackendError::Protocol(msg) => write!(f, "unreadable reply from the provider: {msg}"),
            BackendError::Server { status, message } => {
                write!(f, "the provider failed (HTTP {status}): {message}")
            }
            // Written as it arrives. Every other variant here is a
            // fragment that needs a frame around it; this one is
            // already the sentence the user is owed, and wrapping it in
            // "backend error:" would bury the part they can act on.
            BackendError::Withheld(tell) => f.write_str(tell),
            BackendError::Cancelled => write!(f, "the reply was cancelled by the receiver"),
        }
    }
}

impl Error for BackendError {}
