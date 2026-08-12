//! The supervisor: a local agent that watches, and a remote one that is
//! asked.
//!
//! | module | what it is |
//! |---|---|
//! | [`escalate`] | when the local agent may ask Claude, and when it is told it may not |
//! | [`handoff`] | the one road across the network, with the user's answer on it |
//! | [`seal`] | that road as the only door a remote backend can encode through |
//! | [`watch`] | the event-driven background observer |
//!
//! The shape this implements is written down in `docs/supervisor.md`,
//! and two of its properties decide everything in here.
//!
//! **The local agent is the only one with eyes.** It runs on the user's
//! machine for as long as the desktop does. It can read what the user
//! can read; Claude sees only what the local agent decides to send,
//! after that has been through [`redact`](crate::redact). Claude is
//! never the first responder and never runs unasked.
//!
//! **Neither agent changes anything on its own.** Reading is theirs;
//! writing and deleting belong to the user, one authorisation per
//! action. That half is not in this module — it is the approval path in
//! [`agent::approval`](crate::agent::approval), which was built that way
//! before any of this existed and which this module deliberately does
//! not work around.
//!
//! An escalation is therefore three things in a row, and skipping any of
//! them is the bug this arrangement exists to make hard:
//!
//! ```text
//! Trigger -> Policy::decide -> Gathering (layers 1-2) -> Outgoing (layer 3)
//!         -> Manifest -> the user says yes
//! ```
//!
//! [`handoff`] is that line as a type rather than as a diagram.
//! [`Cleared`](handoff::Cleared) is the only payload in this crate
//! addressed to a remote backend, and the only way to make one is to
//! walk the whole line.
//!
//! [`seal`] is what puts that line where the bytes actually are. A
//! [`Sealed`](seal::Sealed) request is a [`Cleared`](handoff::Cleared)
//! payload in the shape an endpoint accepts, and the remote backend's
//! encoder takes nothing else — so the line above is not a convention a
//! call site can decline to follow.

pub mod escalate;
pub mod handoff;
pub mod seal;
pub mod watch;

pub use escalate::{Attempts, Decision, Grounds, Policy, Remote, Trigger};
pub use handoff::{over_channel, ChannelDiscloser, Cleared, Consent, Discloser, Handoff,
                  NobodyToAsk, PendingDisclosure, Sending};
pub use seal::{payload_of, NotSent, Seal, Sealed};
pub use watch::{Check, Observation, Reporter, Reports, Status, Threshold, Watch, WatchHandle};
