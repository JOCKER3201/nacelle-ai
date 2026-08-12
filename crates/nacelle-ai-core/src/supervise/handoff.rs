//! The one road from the local agent to the remote one.
//!
//! Everything an escalation needs exists in this crate already: the
//! [`Policy`](super::escalate::Policy) that says whether asking is
//! allowed, the [`Outgoing`] that has been through layers 1 to 3, and
//! the [`Disclosure`] that remembers what the user has already been
//! shown. What was missing was the thing that makes them happen **in
//! that order**.
//!
//! ```text
//! Trigger -> Policy::decide -> Gathering (layers 1-2) -> Outgoing (layer 3)
//!         -> Handoff::clear -> the user -> Cleared
//! ```
//!
//! The first arrow of that line is enforced in
//! [`redact`](crate::redact): this module takes an [`Outgoing`], and an
//! [`Outgoing`] is what a [`Gathering`](crate::redact::Gathering)
//! becomes when layer 3 has had its turn. A payload cannot arrive here
//! carrying something added after the last layer that looked at it,
//! because the type it would have had to be added to no longer exists by
//! then.
//!
//! A comment can say that. A type can enforce it, and this one does:
//! [`Cleared`] is the only thing in this crate that hands out a payload
//! addressed to a remote backend, and [`Handoff::clear`] is its only
//! constructor. Building the handoff needs a [`Policy`], which is asked
//! rather than quoted, so a pinned session cannot be escalated by a
//! caller who simply did not ask. Clearing it needs a [`Disclosure`] (so
//! layer 4 is asked whether this is new) and a [`Discloser`] (so when it
//! IS new, the manifest is put in front of somebody). There is no
//! argument that turns any of that off.
//!
//! **The manifest is handed to the interface, not requested by it.**
//! That is the difference between this and an API where the caller asks
//! for a manifest and then decides whether to show it: the second one
//! works right up until somebody writes the call site that does not. The
//! shape here is the one [`Approver`](crate::agent::Approver) already
//! uses for tool changes, for the same reason.
//!
//! **It is not shown every time.** When the payload carries nothing the
//! user has not already seen and this session has escalated before,
//! there is nothing new to disclose and the [`Discloser`] is not
//! troubled. A manifest before every single escalation is one that gets
//! clicked through, which is worse than none: it trains the exact reflex
//! it exists to interrupt.

use std::fmt;
use std::sync::mpsc;
use std::time::Duration;

use crate::redact::manifest::{Disclosure, Manifest};
use crate::redact::{Outgoing, Source};

use super::escalate::{sentence, Decision, Grounds, Policy, Trigger};
use super::seal::Stop;

/// What the user answered to a manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Consent {
    /// Send it.
    Send,
    /// Do not. The reason, when there is one, is repeated back to the
    /// model so it can plan around the refusal rather than proposing the
    /// same escalation again.
    Refuse { reason: Option<String> },
}

impl Consent {
    pub fn refuse(reason: impl Into<String>) -> Self {
        Consent::Refuse {
            reason: Some(reason.into()),
        }
    }

    /// Declined without a reason given.
    pub fn refused() -> Self {
        Consent::Refuse { reason: None }
    }
}

/// Whoever shows the user a manifest and comes back with an answer.
///
/// Implemented for any `FnMut`, so an interface that just wants a rule
/// writes a closure and one that needs state writes a type — the same
/// bargain [`Approver`](crate::agent::Approver) offers.
pub trait Discloser {
    fn disclose(&mut self, manifest: &Manifest) -> Consent;

    /// Give this discloser the turn's stop button.
    ///
    /// Defaulted to nothing, because most disclosers answer immediately
    /// — a closure, a rule, a test. The one that does not is
    /// [`ChannelDiscloser`], which waits on a person and therefore has
    /// to be able to notice that the person gave up on the whole turn
    /// instead of answering the question in front of them.
    fn stops_when(&mut self, _stop: Stop) {}
}

impl<F> Discloser for F
where
    F: FnMut(&Manifest) -> Consent,
{
    fn disclose(&mut self, manifest: &Manifest) -> Consent {
        self(manifest)
    }
}

/// Says no to every manifest.
///
/// The right discloser when there is nobody to ask: a headless run, a
/// widget whose window has gone. It is the only stock one here, and
/// there will not be an `AlwaysSend` — that would be a one-line way to
/// delete layer 4, and anybody who genuinely wants one can write the
/// closure and own it.
pub struct NobodyToAsk;

impl Discloser for NobodyToAsk {
    fn disclose(&mut self, _manifest: &Manifest) -> Consent {
        Consent::refuse(
            "there is nobody at this machine to show the manifest to, so nothing was sent",
        )
    }
}

/// A manifest waiting on the user, sent to whoever owns the screen.
///
/// Answer it with [`PendingDisclosure::send`] or
/// [`PendingDisclosure::refuse`]. Each takes `self`, so an answer
/// cannot be given twice.
///
/// **Dropping it is a refusal**, exactly as with
/// [`PendingApproval`](crate::PendingApproval): an interface that lost
/// the dialog, closed the window or simply forgot must not be able to
/// produce a yes by accident, and the worker waiting on the other end
/// is unblocked either way.
#[derive(Debug)]
pub struct PendingDisclosure {
    manifest: Box<Manifest>,
    reply: mpsc::Sender<Consent>,
}

impl PendingDisclosure {
    /// What is about to leave, in the words the user answers.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Yes.
    pub fn send(self) {
        let _ = self.reply.send(Consent::Send);
    }

    /// No, and why — the reason reaches the model, so it is written for
    /// something that has to change plan rather than retry.
    pub fn refuse(self, reason: impl Into<String>) {
        let _ = self.reply.send(Consent::refuse(reason));
    }

    /// No, without saying why.
    pub fn refused(self) {
        let _ = self.reply.send(Consent::refused());
    }
}

/// The discloser for an agent on a worker thread: it puts the manifest
/// on a channel and blocks until somebody answers.
///
/// Blocking the *worker* is right — there is nothing for it to do until
/// the user decides, and the interface goes on drawing throughout. What
/// it must never do is proceed unasked, so every way this can fail is a
/// refusal: no receiver left, the request dropped instead of answered,
/// the interface gone.
pub struct ChannelDiscloser {
    to: mpsc::Sender<PendingDisclosure>,
    stop: Stop,
}

/// How often a discloser waiting on a manifest looks up to see whether
/// the user gave up on the whole turn instead of answering it.
///
/// The same figure and the same reason as
/// [`APPROVAL_POLL`](crate::agent::worker) one layer up: polling only
/// happens while a manifest is on screen, so the cost is nothing, and it
/// buys the property that a stop works even against an interface that
/// forgot to answer its own question. This was the one wait in the
/// program that did not have it — a plain `recv()`, no timeout, no way
/// out — which meant a stop pressed with a manifest open was not read
/// until somebody closed the manifest.
const DISCLOSURE_POLL: Duration = Duration::from_millis(50);

/// A [`ChannelDiscloser`] and the receiver an interface drains.
///
/// Dropping the receiver is not an error: it turns every later manifest
/// into a refusal, which is what "there is nobody to ask any more"
/// should mean.
pub fn over_channel() -> (ChannelDiscloser, mpsc::Receiver<PendingDisclosure>) {
    let (to, inbox) = mpsc::channel();
    (
        ChannelDiscloser {
            to,
            stop: Stop::never(),
        },
        inbox,
    )
}

impl Discloser for ChannelDiscloser {
    fn disclose(&mut self, manifest: &Manifest) -> Consent {
        let (reply, answer) = mpsc::channel();
        let pending = PendingDisclosure {
            manifest: Box::new(manifest.clone()),
            reply,
        };

        if self.to.send(pending).is_err() {
            return Consent::refuse(
                "there is no interface left to show the manifest to, so nothing was sent",
            );
        }

        loop {
            match answer.recv_timeout(DISCLOSURE_POLL) {
                Ok(consent) => return consent,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.stop.stopped() {
                        return Consent::refuse(
                            "you stopped the turn while the manifest was open, so nothing was sent",
                        );
                    }
                }
                // The request was dropped rather than answered. That is
                // a no — see [`PendingDisclosure`].
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Consent::refuse(
                        "the interface closed the manifest without an answer, so nothing was sent",
                    )
                }
            }
        }
    }

    fn stops_when(&mut self, stop: Stop) {
        self.stop = stop;
    }
}

/// A payload the user has cleared, and the only thing in this crate that
/// hands one out.
///
/// Holds the text rather than the [`Outgoing`] it came from: what is
/// past this point is finished, and a caller that could still add to it
/// could add something the manifest did not describe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cleared {
    destination: String,
    trigger: Trigger,
    payload: String,
    sources: Vec<Source>,
    /// Boxed because it is both the largest thing here and the one that
    /// is usually absent: most escalations in a session carry nothing
    /// new to disclose, and paying for a manifest-sized hole in every
    /// payload to hold the manifest that is not there is backwards.
    shown: Option<Box<Manifest>>,
}

impl Cleared {
    /// The text to send. Layers 1 to 3 have run on it and layer 4 has
    /// been answered.
    pub fn payload(&self) -> &str {
        &self.payload
    }

    /// Where it is going — the backend's name, never a URL.
    pub fn destination(&self) -> &str {
        &self.destination
    }

    pub fn bytes(&self) -> usize {
        self.payload.len()
    }

    /// Why this was escalated, for the interface that wants to say so.
    pub fn trigger(&self) -> &Trigger {
        &self.trigger
    }

    /// The files whose contents are in it.
    pub fn sources(&self) -> &[Source] {
        &self.sources
    }

    /// The manifest the user was shown, when they were shown one.
    ///
    /// `None` means there was nothing new to disclose, not that nothing
    /// was disclosed: this session has escalated before and every file
    /// in this payload was in one the user already answered.
    pub fn manifest(&self) -> Option<&Manifest> {
        self.shown.as_deref()
    }
}

/// How an escalation ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sending {
    /// It may go.
    Cleared(Cleared),
    /// The policy would not allow it — pinned, no credential, no route.
    /// `tell` is what the agent says out loud, and it always opens with
    /// what could not be done here.
    Refused { grounds: Grounds, tell: String },
    /// The user saw what would have left and said no.
    Declined { tell: String },
}

impl Sending {
    pub fn cleared(&self) -> Option<&Cleared> {
        match self {
            Sending::Cleared(cleared) => Some(cleared),
            _ => None,
        }
    }

    /// What the agent says, when the answer was no. This goes back to
    /// the model as well as to the screen — a model told only "error"
    /// tries the same thing again.
    pub fn tell(&self) -> Option<&str> {
        match self {
            Sending::Cleared(_) => None,
            Sending::Refused { tell, .. } | Sending::Declined { tell } => Some(tell),
        }
    }

    pub fn is_cleared(&self) -> bool {
        matches!(self, Sending::Cleared(_))
    }
}

/// The sentence a decline always contains, whatever reason follows it.
///
/// Fixed, like the refusal sentence in
/// [`escalate`](super::escalate): the user having said no and the policy
/// having said no leave the agent in the same place, and the agent
/// should not sound more disappointed about one than the other.
const NOTHING_LEFT: &str =
    "You saw what would have been sent and said no, so nothing left this machine.";

/// An escalation that has been assembled and not yet cleared.
///
/// Built from the three things an escalation is made of. It cannot be
/// built from fewer, and there is no method on it that produces a
/// payload except [`Handoff::clear`].
#[derive(Clone, Debug)]
pub struct Handoff {
    decision: Decision,
    destination: String,
    outgoing: Outgoing,
}

impl Handoff {
    /// Assemble one, asking the policy whether it is allowed at all.
    ///
    /// The policy is consulted HERE rather than taken as an already-made
    /// [`Decision`], and that is the whole difference between a pin that
    /// holds and a pin that can be walked around: `Decision::Ask` is an
    /// ordinary public variant anybody can write down, and a constructor
    /// that accepted one would let a caller who never asked the policy
    /// build an escalation the user had forbidden. Deciding is cheap and
    /// has no side effects, so there is nothing to save by passing the
    /// answer in.
    ///
    /// `destination` is the backend's name, and it is what the user
    /// reads on the manifest — never a URL, and never anything derived
    /// from a credential.
    pub fn new(
        policy: &Policy,
        trigger: Trigger,
        destination: impl Into<String>,
        outgoing: Outgoing,
    ) -> Self {
        Handoff {
            decision: policy.decide(trigger),
            destination: destination.into(),
            outgoing,
        }
    }

    /// What is about to leave, before anybody has agreed to it.
    ///
    /// For an interface that wants to show a size or a file count next
    /// to the button. It is the redacted text — layers 1 to 3 have
    /// already run — which is why looking at it here is safe and sending
    /// it from here is not: only [`Cleared`] means somebody agreed.
    pub fn preview(&self) -> &Outgoing {
        &self.outgoing
    }

    /// Why this would be escalated at all.
    pub fn trigger(&self) -> &Trigger {
        self.decision.trigger()
    }

    /// Layer 4, and then send or do not.
    ///
    /// Consumes the handoff, so the un-cleared version cannot be kept
    /// alongside the cleared one and sent by mistake. The disclosure is
    /// recorded only after the answer, and only when there was an answer
    /// — recording one that did not happen would suppress the next
    /// manifest, which is the one thing layer 4 must never do.
    pub fn clear(self, disclosure: &mut Disclosure, discloser: &mut dyn Discloser) -> Sending {
        let trigger = match self.decision {
            Decision::Stay { grounds, tell, .. } => return Sending::Refused { grounds, tell },
            Decision::Ask { trigger } => trigger,
        };

        let shown = match disclosure.required_for(self.outgoing.sources()) {
            None => None,
            Some(why) => {
                let manifest = self
                    .outgoing
                    .manifest(&self.destination, &trigger.reason(), why);
                match discloser.disclose(&manifest) {
                    Consent::Refuse { reason } => {
                        return Sending::Declined {
                            tell: declined(&trigger, reason.as_deref()),
                        }
                    }
                    Consent::Send => {
                        disclosure.accepted(self.outgoing.sources());
                        Some(Box::new(manifest))
                    }
                }
            }
        };

        Sending::Cleared(Cleared {
            destination: self.destination,
            trigger,
            // The same string the manifest was measured against, not a
            // second rendering of the same pieces: an [`Outgoing`] holds
            // one finished payload and this is a copy of it.
            payload: self.outgoing.payload().to_string(),
            sources: self.outgoing.sources().to_vec(),
            shown,
        })
    }
}

/// What the agent says when the user declined the manifest.
///
/// Opens with what could not be done here, exactly as a policy refusal
/// does. The user knows what they just said no to; what they may not
/// know is what it cost, and the model needs that sentence in order to
/// plan around the refusal instead of proposing it again.
fn declined(trigger: &Trigger, reason: Option<&str>) -> String {
    let mut tell = format!("{} {NOTHING_LEFT}", sentence(&trigger.shortfall()));
    if let Some(reason) = reason {
        let reason = reason.trim();
        if !reason.is_empty() {
            tell.push_str(&format!(" You said: {reason}"));
        }
    }
    tell
}

impl fmt::Display for Sending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Sending::Cleared(cleared) => write!(
                f,
                "cleared to send {} bytes to {}",
                cleared.bytes(),
                cleared.destination()
            ),
            Sending::Refused { tell, .. } | Sending::Declined { tell } => f.write_str(tell),
        }
    }
}
