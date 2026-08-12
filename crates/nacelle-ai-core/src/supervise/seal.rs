//! The door: layers 2, 3 and 4 on the way to a provider that is not on
//! this machine.
//!
//! [`redact`](crate::redact) has the layers and [`handoff`](super::handoff)
//! has the road, and until this module existed neither was on the path
//! of anything. A remote backend took a [`Request`] and encoded it, and
//! the four layers sat beside that path being correct at nothing. Code
//! like that is worse than no protection at all: the program *looks*
//! guarded, and an absent guard at least looks absent.
//!
//! So this is the door, and it is the shape of a door rather than a
//! rule: [`body`](crate::backend::anthropic) — the only encoder in this
//! crate that produces bytes for a socket — takes a [`Sealed`], and the
//! only way to make one is [`Seal::seal`], which runs
//!
//! ```text
//! Policy::decide -> layer 2 (scan) -> layer 3 (review) -> layer 4 (manifest) -> Sealed
//! ```
//!
//! every time, in that order, with no argument that turns any of it off.
//! A second code path in a backend cannot skip the layers, because there
//! is nothing for it to encode until it has been through them. That is
//! the difference between a comment and a guarantee.
//!
//! **The local backend has no seal, and that is deliberate.** Layers 2
//! to 4 exist because bytes are about to leave the machine. Nothing
//! leaves when the model is Ollama on the same host, so redacting there
//! would cost the user answers about their own files in exchange for
//! nothing — the local model reading a key it was asked about is the
//! machine's owner reading their own key. See
//! [`Backend::is_local`](crate::backend::Backend::is_local), which is
//! how the two halves are told apart everywhere else too.
//!
//! **What layer 4 can say about a request, and what it cannot.** A
//! manifest names the files whose contents are in the payload, and a
//! [`Request`] does not record where its text came from — a tool result
//! is a string, and nothing in it says what it was read from. What the
//! request *does* record is the call each result answers, and a call
//! carries its arguments: so a result whose call passed a path to a tool
//! this program declared is a file with a name, and
//! [`files_named_by`] names it. A file the model quoted into its own
//! prose has no name here, and every manifest says so rather than
//! letting the list read as complete.
//!
//! That list is not decoration. Layer 4 shows a manifest before the
//! first escalation of a session **and** whenever the payload carries a
//! file the user has not already answered for, and the second rule was
//! unreachable while the list was hard-coded empty: after one yes, layer
//! 4 was never asked again for the rest of the session. Everything else
//! on the manifest — the size after redaction, what was cut and how much
//! of it, whether a local model reviewed it, where it is going and why —
//! is exact.
//!
//! **One walk, one number.** The size the user is shown is the length of
//! [`payload_of`] over the request that is about to be encoded, taken
//! after every layer has run and after the withheld note has been put
//! in. Nothing is tallied up alongside the redaction as it goes, because
//! a tally is a second account of the same thing and the day the two
//! disagree is the day the manifest lies. [`Sealed::bytes`] recomputes
//! it from the sealed request rather than remembering it, so the number
//! cannot outlive the thing it describes.
//!
//! **The door is where a stop has to be read.** Everything above takes
//! time — layer 3 is a whole turn against a local model and layer 4
//! waits on a person — and none of it produces an event, so
//! [`Flow::Stop`](crate::backend::Flow) cannot reach into it: the sink
//! is not called until the *reply* is being decoded, which is after the
//! socket. [`Seal::stops_when`] is how a caller hands the door its stop
//! button, and it is asked between the layers and by the discloser while
//! it waits. Without it, a turn the user stopped during layer 3 was
//! shown as stopped and posted anyway.
//!
//! ## What is scanned, what is checked, and what is neither
//!
//! Everything a request carries as *text* goes through layer 2, and the
//! marker it leaves behind is the point of it: the far model reads "an
//! Anthropic API key was removed here" and asks the user for the value
//! instead of answering the wrong question.
//!
//! A handful of strings cannot take a marker. A tool call is dispatched
//! on its name, matched to its result by its id, and read by its
//! argument names — put a marker in any of those and the call does not
//! get safer, it gets impossible. That was the argument for leaving them
//! alone, and it is a good one, but it only ever defended the names
//! **the tool registry declared**. A tool name the registry has never
//! heard of dispatches to nothing. An argument name no schema declares
//! is read by nothing. Those two are not carrying a call's meaning; they
//! are strings a model wrote, in the one place nothing was looking.
//!
//! So they are checked rather than redacted, and the answer to a hit is
//! that **the turn does not go**:
//!
//! | string | what happens to it |
//! |---|---|
//! | [`Request::model`] | layer 2 reads it and a hit stops the turn; it is counted, never edited — a marker in it is a 404 |
//! | a thinking block's signature | layer 2 reads it for *named* shapes and a hit stops the turn; counted, never edited — the endpoint verifies it |
//! | tool call and result ids | rewritten: the copy that is encoded carries `toolu_0001`, `toolu_0002` |
//! | a tool name the registry declared | layer 2 reads it, and anything it finds stops the turn |
//! | a property name in a schema this program declares | the same |
//! | an argument name that schema declares | nothing: this program's own constant, which cannot hold anything read off this machine |
//! | any other tool name or argument name | layer 2 reads it, and anything it finds stops the turn |
//!
//! Three of those rows are recent and each one was a hole that had been
//! measured going out: a `signature` was in no walk at all and the
//! encoder wrote it verbatim; a declaration's *schema keys* were read by
//! nothing, while its name and a call's argument names were both read;
//! and `Request::model` was neither read nor counted.
//!
//! The registry is read off the [`Request`] itself, because a request
//! carries the tools its agent offered. Nothing here has to be told what
//! the agent above it knows, and nothing here can drift out of step with
//! it.
//!
//! **Why refusal, and not a marker or a dropped block.** Three roads
//! were open for a name the registry does not know.
//!
//! *Redact it.* A marker in the name takes the key out and leaves a call
//! named `[[redacted: …]]`. It is the smallest change and the weakest
//! outcome: the block still travels, still costs context, and now says
//! something the far model will try to make sense of — while the thing
//! being preserved, a name that dispatches to nothing, was never worth
//! preserving.
//!
//! *Drop the block.* Cutting the `tool_use` out of the history is worse
//! than either. A `tool_use` with no matching `tool_result`, or a
//! `tool_result` whose call has gone, is a conversation the endpoint
//! rejects — so repairing it means editing a second block to hide what
//! was done to the first, which is this module quietly rewriting the
//! user's history to cover for their local model.
//!
//! *Refuse the turn.* Nothing is sent, the user is told what kind of
//! thing was found and where, and the sentence never repeats it. This is
//! the one that fits what the tool is for. The local model is what is
//! being guarded against here — it is the half that reads the user's
//! files — and a local model that has put a credential inside a tool
//! *name* is not having an off day, it is doing the exact thing these
//! layers exist to stop. Redaction answers that by sending the rest of
//! the turn anyway. Refusal answers it by not sending, which costs one
//! turn and nothing else, because the call it refused could not have run
//! here in the first place.
//!
//! **What is refused is a shape, not an unknown name.** The check does
//! not stop on a name the registry lacks; it stops on a credential
//! *inside* one. That difference is the difference between a guard and a
//! trap. A small local model invents tool names constantly, and an
//! invented name is in the history before anything tries to run it — so
//! a rule that stopped on unknown names would let one hallucination end
//! the session's ability to reach Claude at all, over a string that was
//! never a secret. An unknown name is therefore read like any other text
//! and travels when it is clean.
//!
//! **Identifiers stopped being a question.** An id is opaque by
//! construction: `toolu_01…` is a long run with no natural-language
//! profile, which is precisely what layer 2's entropy rule looks for. So
//! the entropy rule could never be consulted about one — it would end
//! sessions over ids doing nothing but their job — and an id was held to
//! the *named* shapes only. Measured, that let through a
//! forty-four-character opaque secret and
//! `toolu_01_michael_furtak_1981_04_02_krakow`, both of which layer 2
//! cuts out of prose. The field is not judged any more; it is
//! **rewritten**. The endpoint needs an id to be unique in the request
//! and to match between a call and its result, and a counter is both, so
//! whatever the local model put there is not what leaves. See
//! [`local_identifiers`].
//!
//! **A credential written across two blocks.** Layer 2 is handed one
//! string at a time and cannot see a key the model split over four of
//! them. What answers that is [`continuation_of`], and it is the
//! block-level twin of layer 2's own wrapped-line rule: when a block
//! ends at a named credential that was cut, a bare run of key characters
//! at the head of the next block is the rest of it and goes too.
//!
//! **What this still does not see.** Layer 3 reads [`payload_of`], and
//! that walk is over values, so the local reviewer is never shown a
//! name or an argument name. Layer 2's shapes are all these strings are
//! held to. A secret with no recognisable shape sitting in an argument
//! name is not caught here — it would not have been caught in a value
//! either, but in a value layer 3 would at least have had the chance to
//! speak about it. And the block-continuation rule needs a first block
//! that layer 2 could see: a key split so that its first piece is under
//! the prefix rule's own length still goes.
//!
//! The walk is over the [`Request`], not over what the encoder happens
//! to emit from it today, and [`each_string`] is the one list of the
//! fields it has. An encoder that starts sending a field it used to drop
//! therefore cannot open a hole that nobody notices — which is what the
//! signature hole was, and it existed because there were two walks and
//! only one of them had been kept up to date.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use serde_json::Value;

use crate::error::BackendError;
use crate::message::{Content, Request};
use crate::redact::manifest::{Disclosure, Manifest};
use crate::redact::review::{self, Removal, Reviewer};
use crate::redact::scan::{self, Finding, Kind};
use crate::redact::{Outgoing, Source, WITHHELD};

use super::escalate::{Decision, Grounds, Policy, Trigger};
use super::handoff::{Cleared, Discloser, Handoff, Sending};

/// The stop button, as the door can read it.
///
/// **Why the door needs one at all.** A turn's `Cancel` used to be read
/// in exactly two places: at the head of the worker's loop, before the
/// turn starts, and inside the sink — and the sink is not called until
/// the *reply* is being decoded, which is after `transport.post`.
/// Everything between those two is a window in which a stop is not
/// observed and the payload leaves anyway, and this module is what is in
/// that window: layer 3 is a whole turn against a local model, seconds
/// of it, and layer 4 blocks on a person. Measured: a reviewer that
/// pressed stop while it was thinking produced `turn outcome:
/// Failed(Cancelled)` on the interface and `requests that reached the
/// socket: 1` on the transport. The interface showed the turn as
/// stopped; the bytes had gone.
///
/// It is a closure rather than a `Cancel` because the door has no
/// business knowing about turns: what it needs to ask is "should I still
/// be doing this", and the worker is the only thing that knows why the
/// answer is no.
#[derive(Clone)]
pub struct Stop(Arc<dyn Fn() -> bool + Send + Sync>);

impl Stop {
    pub fn new(asked: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Stop(Arc::new(asked))
    }

    /// The one for a caller with no stop button. Named rather than
    /// `Default`, because "nothing can stop this" should be something a
    /// reader sees written down.
    pub fn never() -> Self {
        Stop::new(|| false)
    }

    pub fn stopped(&self) -> bool {
        (self.0)()
    }
}

impl fmt::Debug for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Stop({})", self.stopped())
    }
}

/// What the agent says when the user stopped the turn before it left.
///
/// It is not a refusal and not a failure of the provider's: the person
/// at the machine changed their mind while the local half was still
/// working, and the only thing worth saying is that they were in time.
const STOPPED: &str =
    "You stopped this turn while it was still being checked, so nothing left this machine.";

/// Nothing was sent, and what the agent says about it.
///
/// The request never reached a socket, for one of three reasons: the
/// policy would not have it — pinned, no credential, nothing answering —
/// or the user read the manifest and said no, or the payload itself was
/// one this module will not send whatever anyone says about it. All
/// three leave the agent in the same place, which is why they are one
/// type with one sentence in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotSent {
    /// Why the policy refused, or `None` when the policy was willing and
    /// the payload was refused anyway — by the user reading the
    /// manifest, or by this module. [`Grounds`] is about whether this
    /// session may reach the remote half at all, and neither of those
    /// two says anything about that: the next turn may well go.
    pub grounds: Option<Grounds>,
    /// What the agent says out loud. Always opens with what could not
    /// be done here, because that is the part the user and the model
    /// both have to act on.
    pub tell: String,
}

impl fmt::Display for NotSent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.tell)
    }
}

impl std::error::Error for NotSent {}

impl From<NotSent> for BackendError {
    fn from(refused: NotSent) -> Self {
        BackendError::Withheld(refused.tell)
    }
}

/// A request that has been through the layers, and the only thing a
/// remote backend in this crate can encode.
///
/// Its fields are private and its only constructor is [`Seal::seal`].
/// Holding one is therefore proof of four things at once: the policy
/// allowed the escalation, every string in it was scanned, the local
/// model was given its chance to remove more, and the user has either
/// seen a manifest and agreed or had nothing new to be shown.
#[derive(Clone, Debug, PartialEq)]
pub struct Sealed {
    request: Request,
    cleared: Cleared,
}

impl Sealed {
    /// The request to encode — the same conversation, with the same
    /// shape, minus whatever the layers took out.
    pub fn request(&self) -> &Request {
        &self.request
    }

    /// Where it is going: the backend's name, never a URL.
    pub fn destination(&self) -> &str {
        self.cleared.destination()
    }

    /// Why this went to the remote model at all.
    pub fn trigger(&self) -> &Trigger {
        self.cleared.trigger()
    }

    /// The manifest the user was shown, when they were shown one.
    ///
    /// `None` means there was nothing new to disclose — this session
    /// has escalated before and this payload carries no file the user
    /// has not already answered for.
    pub fn manifest(&self) -> Option<&Manifest> {
        self.cleared.manifest()
    }

    /// How much text this carries, after redaction. Not the size of the
    /// encoded body — the wire format's envelope is not the user's
    /// business, and it is their text that the number is about.
    ///
    /// Counted off the request that is about to be encoded, by the walk
    /// the layers themselves used, rather than remembered from when the
    /// manifest was built. The two are therefore the same number by
    /// construction instead of by agreement — see [`payload_of`].
    pub fn bytes(&self) -> usize {
        payload_of(&self.request).len()
    }
}

/// Everything a remote backend needs in order to be allowed to speak,
/// held together so that it cannot be assembled by halves.
///
/// One per session. It carries the state layer 4 needs — what the user
/// has already been shown — and the state the policy needs, which is
/// why a backend holds one for its whole life rather than building one
/// per turn: a fresh seal every turn would show the first-escalation
/// manifest every turn, and a manifest shown every turn is one that
/// gets clicked through.
pub struct Seal {
    destination: String,
    policy: Policy,
    trigger: Trigger,
    disclosure: Disclosure,
    /// `None` when no local model is loaded. Absent rather than
    /// [`NoReview`](crate::redact::NoReview) on purpose: the manifest
    /// has to be able to tell the user that layer 3 did not run, and a
    /// reviewer that always answers "nothing to remove" is
    /// indistinguishable on a manifest from one that looked.
    reviewer: Option<Box<dyn Reviewer + Send>>,
    discloser: Box<dyn Discloser + Send>,
    /// Asked between the layers, and by the discloser while it waits.
    /// [`Stop::never`] until somebody hands over a real one, because a
    /// seal built by a test or a script has nobody to be stopped by.
    stop: Stop,
}

impl Seal {
    /// A seal for one session.
    ///
    /// `destination` is the backend's name and it is what the user
    /// reads on the manifest — never a URL, never anything derived from
    /// a credential. `trigger` is why the remote half is being reached
    /// for at all; it is what the manifest gives as the reason, and
    /// [`Seal::escalated_because`] replaces it when the next turn has a
    /// different one.
    ///
    /// There is no constructor without a [`Discloser`]. An escalation
    /// nobody can be asked about is refused, not waved through, and
    /// [`NobodyToAsk`](super::handoff::NobodyToAsk) is the honest way to
    /// say there is nobody.
    pub fn new(
        destination: impl Into<String>,
        policy: Policy,
        trigger: Trigger,
        discloser: impl Discloser + Send + 'static,
    ) -> Self {
        Seal {
            destination: destination.into(),
            policy,
            trigger,
            disclosure: Disclosure::new(),
            reviewer: None,
            discloser: Box::new(discloser),
            stop: Stop::never(),
        }
    }

    /// Give the door a stop button.
    ///
    /// It is handed to the discloser as well, because layer 4 is the
    /// longest wait of the three and the only one that waits on a
    /// person: [`ChannelDiscloser`](super::handoff::ChannelDiscloser)
    /// used to block in `recv()` with no timeout and no way out, so a
    /// stop pressed while a manifest was on screen was not read until
    /// somebody answered the manifest.
    pub fn stops_when(&mut self, stop: Stop) {
        self.discloser.stops_when(stop.clone());
        self.stop = stop;
    }

    /// Give layer 3 a model to ask.
    ///
    /// It has to be a local one — see
    /// [`LocalReviewer`](crate::redact::LocalReviewer), which will not
    /// be built over a backend that is not, since asking a remote model
    /// whether a payload may be sent sends it.
    pub fn with_reviewer(mut self, reviewer: impl Reviewer + Send + 'static) -> Self {
        self.reviewer = Some(Box::new(reviewer));
        self
    }

    /// Why the next turn is going off the machine.
    pub fn escalated_because(&mut self, trigger: Trigger) {
        self.trigger = trigger;
    }

    pub fn trigger(&self) -> &Trigger {
        &self.trigger
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// Pin the session to the local model. Every later turn on this
    /// backend is refused before a socket is opened.
    pub fn pin(&mut self) {
        self.policy.pin();
    }

    pub fn unpin(&mut self) {
        self.policy.unpin();
    }

    /// Tell the policy what a failed turn said about the remote half.
    ///
    /// This is how "no network degrades exactly like a pin" stops being
    /// a promise and becomes what the code does: a backend that hands
    /// its failures here stops reaching for a provider that has already
    /// stopped answering, and says the same kind of sentence about it
    /// that a pin does. See [`Policy::observe`].
    pub fn observe(&mut self, error: &BackendError) {
        self.policy.observe(error);
    }

    /// One line an interface can show: whether this session can reach
    /// the remote half at all, and why not when it cannot.
    pub fn status(&self) -> String {
        self.policy.status()
    }

    /// Layers 2, 3 and 4, and then either a request that may be encoded
    /// or the sentence explaining why there is none.
    ///
    /// Takes `&mut self` because layer 4 remembers: the user is asked
    /// before the first thing leaves this session and again when
    /// something new turns up, and neither is knowable without keeping
    /// the record.
    pub fn seal(&mut self, request: &Request) -> Result<Sealed, NotSent> {
        // Asked before the layers rather than after them, so a pinned
        // session does not wake a local model to review a payload that
        // is not going anywhere. `Handoff::new` asks again below and
        // its answer is the one that decides — deciding is cheap and
        // has no side effects, and this is an early exit rather than a
        // second authority.
        if let Decision::Stay { grounds, tell, .. } = self.policy.decide(self.trigger.clone()) {
            return Err(NotSent {
                grounds: Some(grounds),
                tell,
            });
        }

        // The strings layer 2 cannot put a marker in, checked instead of
        // redacted. Before `layers` on purpose: there is no sense waking
        // a local model to review a payload, or putting a manifest in
        // front of the user, when the answer is the same whatever either
        // of them says.
        if let Some(tell) = unsendable(request) {
            return Err(NotSent {
                grounds: None,
                tell,
            });
        }

        // Before layer 3, which is a whole turn against a local model,
        // and again after it. A stop that arrives during those seconds
        // used to be read for the first time when the *reply* was being
        // decoded — which is after the request had gone.
        if self.stop.stopped() {
            return Err(NotSent {
                grounds: None,
                tell: STOPPED.to_string(),
            });
        }

        let (redacted, outgoing) = layers(request, self.reviewer.as_deref_mut());

        if self.stop.stopped() {
            return Err(NotSent {
                grounds: None,
                tell: STOPPED.to_string(),
            });
        }

        let handoff = Handoff::new(
            &self.policy,
            self.trigger.clone(),
            &self.destination,
            outgoing,
        );
        match handoff.clear(&mut self.disclosure, &mut *self.discloser) {
            // Asked once more after layer 4, because layer 4 waits on a
            // person and a person takes as long as they take. What is
            // left between here and `transport.post` is encoding a JSON
            // body, which is not a window anybody can press a button
            // inside of.
            Sending::Cleared(_) if self.stop.stopped() => Err(NotSent {
                grounds: None,
                tell: STOPPED.to_string(),
            }),
            Sending::Cleared(cleared) => Ok(Sealed {
                request: redacted,
                cleared,
            }),
            Sending::Refused { grounds, tell } => Err(NotSent {
                grounds: Some(grounds),
                tell,
            }),
            Sending::Declined { tell } => Err(NotSent {
                grounds: None,
                tell,
            }),
        }
    }
}

/// Written out rather than derived: a `Box<dyn Discloser>` has no
/// `Debug` and should not grow one, and what is worth printing about a
/// seal is what it would allow, not what it is made of.
impl fmt::Debug for Seal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Seal")
            .field("destination", &self.destination)
            .field("status", &self.policy.status())
            .field("trigger", &self.trigger.reason())
            .field("reviewed", &self.reviewer.is_some())
            .field("disclosed", &self.disclosure.has_shown())
            .finish()
    }
}

/// Layers 2 and 3 over a whole request.
///
/// Returns the request with every string redacted in place — the shape
/// the endpoint wants, which one joined-up payload could not be turned
/// back into — and the [`Outgoing`] that accounts for what came out of
/// it, which is what layer 4 puts on screen.
fn layers(
    request: &Request,
    // Spelled out to the last bound because the seal owns its reviewer
    // for the whole session: `&mut` is invariant, so an elided object
    // lifetime here would be the one thing the borrow checker cannot
    // reconcile with a field that outlives the call.
    reviewer: Option<&mut (dyn Reviewer + Send + 'static)>,
) -> (Request, Outgoing) {
    let mut redacted = request.clone();

    // Before layer 2, because it removes a field from layer 2's problem
    // rather than solving it there.
    local_identifiers(&mut redacted);

    // Layer 2. Deterministic, and the same on every payload.
    //
    // The blocks are walked in the order the endpoint reads them, and
    // what a block ends with is carried into the next one — see
    // [`continuation_of`]. Layer 2 itself reads one string at a time,
    // which is what let a key survive by being written across four of
    // them.
    let mut findings: Vec<Finding> = Vec::new();
    let mut carried: Option<Kind> = None;
    each_text(&mut redacted, &mut |text| {
        let cut = scan::scan(text);
        let mut out = cut.text;

        if let Some(kind) = carried.take() {
            if let Some((from, to)) = continuation_of(&out) {
                findings.push(Finding {
                    kind: kind.clone(),
                    at: from,
                    bytes: to - from,
                });
                out = format!("{}{}{}", &out[..from], scan::marker(&kind), &out[to..]);
                carried = Some(kind);
            }
        }
        // A block whose last thing is a named credential is a block a
        // credential may run off the end of.
        if carried.is_none() {
            carried = ends_with_a_named_cut(&out, &cut.findings);
        }

        *text = out;
        findings.extend(cut.findings);
    });

    // Layer 3. The model sees the payload as one piece of text, because
    // meaning does not stop at a message boundary; what it asks for is
    // then taken out of the pieces one at a time, and only what actually
    // came out is recorded. A quote that spanned two messages and so
    // matched neither is a review that did nothing, not a manifest line
    // claiming a removal that never happened.
    let mut removals: Vec<Removal> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let reviewed = reviewer.is_some();
    if let Some(reviewer) = reviewer {
        let review = reviewer.review(&payload_of(&redacted));
        if let Some(note) = review.note.clone() {
            notes.push(note);
        }
        each_text(&mut redacted, &mut |text| {
            let (out, done) = review::apply(text, &review);
            *text = out;
            for removal in done {
                if !removals.contains(&removal) {
                    removals.push(removal);
                }
            }
        });
    }

    if !findings.is_empty() || !removals.is_empty() {
        say_something_was_withheld(&mut redacted);
    }

    // Last, and off the finished request rather than off anything
    // collected on the way through it. Every string the endpoint will be
    // given is in this walk — the withheld note included, in the system
    // prompt where it actually ends up — so the size the user reads is
    // the size of what is encoded a moment later, and not a figure that
    // was true earlier in this function.
    let body = payload_of(&redacted);
    let sources = files_named_by(request, &redacted);

    (
        redacted,
        Outgoing::from_parts(body, sources, findings, removals, reviewed, notes),
    )
}

/// The files whose contents are in this payload, as far as the request
/// itself can say.
///
/// **Why this exists.** Layer 4 has two rules for when to put a manifest
/// in front of the user: the first escalation of a session, and a
/// payload carrying a file they have not already answered for. The
/// second rule was dead code. `Outgoing::from_parts` set the source list
/// to empty, so `Disclosure::required_for` never found an unseen file,
/// so after one yes the user was never asked again — measured, a second
/// turn carrying `here is my diary: 1998-04-02, the diagnosis was
/// confirmed` reached the socket with no manifest shown and nothing on
/// screen to say it had.
///
/// **What can be known and what cannot.** A [`Request`] records no
/// provenance: a tool result is a string, and nothing in it says where
/// it was read from. What the request *does* record is the call the
/// result answers, and a call carries its arguments — so a result whose
/// call passed a path to a tool this program declared is a file, and it
/// is named here. A file the model quoted into its own prose is not, and
/// the note on every manifest says so rather than letting the list read
/// as complete.
///
/// The counts are read off the payload, never accumulated beside it:
/// what came out is the difference between the result as it was and the
/// result as it will leave, and what was removed is the number of
/// markers in it.
fn files_named_by(original: &Request, redacted: &Request) -> Vec<Source> {
    let vocabulary = Vocabulary::of(original);

    // Which call read which file. Only a tool this program declared, and
    // only an argument its own schema names: a path invented by the
    // model under a name nothing declared is not evidence of a file
    // having been read.
    let mut read: Vec<(&str, &str)> = Vec::new();
    for message in &original.messages {
        for content in &message.content {
            let Content::ToolUse(call) = content else {
                continue;
            };
            let Some(declared) = vocabulary.arguments(&call.name) else {
                continue;
            };
            if let Some(path) = path_argument(&call.input, declared) {
                read.push((call.id.as_str(), path));
            }
        }
    }
    if read.is_empty() {
        return Vec::new();
    }

    let mut sources: Vec<Source> = Vec::new();
    for (at, message) in original.messages.iter().enumerate() {
        for (index, content) in message.content.iter().enumerate() {
            let Content::ToolResult { id, output, .. } = content else {
                continue;
            };
            let Some((_, path)) = read.iter().find(|(call, _)| call == id) else {
                continue;
            };
            let sent = match redacted
                .messages
                .get(at)
                .and_then(|message| message.content.get(index))
            {
                Some(Content::ToolResult { output, .. }) => output,
                // The copy is the original with strings edited in place,
                // so this cannot happen; if it ever does, the honest
                // answer is the larger number.
                _ => output,
            };
            sources.push(Source {
                path: std::path::PathBuf::from(path),
                bytes_read: output.len(),
                bytes_sent: sent.len(),
                removed: sent.matches("[[redacted:").count(),
            });
        }
    }
    sources
}

/// The path a call was given, if it was given one under a name its own
/// schema declares.
fn path_argument<'a>(input: &'a Value, declared: &BTreeSet<&str>) -> Option<&'a str> {
    let Value::Object(map) = input else {
        return None;
    };
    map.iter().find_map(|(key, value)| {
        let names_a_file = key == "path" || key == "file" || key.ends_with("_path");
        (names_a_file && declared.contains(key.as_str()))
            .then(|| value.as_str())
            .flatten()
    })
}

// ------------------------------------- a credential written across blocks

/// The shortest stretch of credential characters worth calling the rest
/// of something. The same figure [`scan`](crate::redact::scan) uses for
/// the line under a wrapped value, for the same reason: shorter than
/// this and a marker over an ordinary short word is noise that teaches
/// the user to stop reading markers.
const CONTINUATION_MIN: usize = 12;

/// Where the rest of the previous block's credential is in this one, if
/// it is here at all.
///
/// **What this closes.** Layer 2 is handed one string at a time, so a
/// key split over four text blocks is in none of them: measured,
/// twenty-seven characters per block, three of the four blocks on the
/// wire verbatim — eighty-one of a hundred and eight characters of the
/// key, the whole random body among them. Every rule in layer 2 looked
/// at each piece and passed, because each piece was short, of few
/// character classes, and had no prefix. Layer 3 reads the joined
/// payload and could in principle have seen it, but layer 3 is a model
/// and it is optional: with no local model loaded, nothing was looking.
///
/// **Why this shape and not a reassembly of the whole payload.** Joining
/// every stretch of credential characters in a request and asking what
/// it spells is a rule that answers wrongly on ordinary conversations —
/// measured: a payload carrying the same key in a system prompt, a tool
/// argument, a tool result and a tool description reads as one
/// hundred-and-thirty-character credential running from the first block
/// to the third, because two copies of a key concatenate into something
/// longer than either. This asks a narrower question with a precondition
/// instead, exactly as
/// [`wrapped_tails`](crate::redact::scan) does one line lower down:
/// something on the block above has ALREADY been judged a named
/// credential and ALREADY been cut at the end of that block, and all
/// that is left to decide is how far it reaches.
///
/// **What counts as the rest of it.** One stretch of key characters —
/// letters, digits, `-` and `_`, and nothing else, so a path or a
/// version number is not it — of at least [`CONTINUATION_MIN`]
/// characters, in more than one character class, with no word of any
/// language in front of it. `b: 1Ws9Yb3Cd6Ef0Gh5Ij8Kl2Mn4Op` qualifies;
/// `look at my config` does not, and neither does
/// `/home/michael/.config/nacelle`.
fn continuation_of(text: &str) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut at = 0usize;
    while at < bytes.len() {
        if !is_key_byte(bytes[at]) {
            at += 1;
            continue;
        }
        let start = at;
        while at < bytes.len() && is_key_byte(bytes[at]) {
            at += 1;
        }
        let run = &text[start..at];
        // Asked before the run is judged: a word in front means the
        // block went back to being somebody's sentence, and a sentence
        // is not the second half of a key. It is asked first because a
        // long word passes the length and class tests below —
        // `CreateWidget` is twelve characters in two classes.
        if run.len() >= 3 && run.bytes().all(|b| b.is_ascii_alphabetic()) {
            return None;
        }
        if run.len() >= CONTINUATION_MIN && key_classes(run) >= 2 {
            return Some((start, at));
        }
    }
    None
}

/// Whether a byte can be part of a key's body: what every provider in
/// [`scan`](crate::redact::scan)'s table writes its keys in, and nothing
/// that makes a path, a host name or a version number.
fn is_key_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

/// How many of upper, lower and digit a run uses.
fn key_classes(run: &str) -> u32 {
    [
        run.bytes().any(|b| b.is_ascii_lowercase()),
        run.bytes().any(|b| b.is_ascii_uppercase()),
        run.bytes().any(|b| b.is_ascii_digit()),
    ]
    .into_iter()
    .map(u32::from)
    .sum()
}

/// What a block's last cut was, when the block ends at it and it was a
/// credential with a name.
///
/// The entropy rule is not enough of a precondition to reach past a
/// block boundary on: it is the rule with judgement in it, and a block
/// that ends in a long opaque string ends in one rather often. A named
/// shape — a provider prefix, an armour line, a JWT — is a fact.
fn ends_with_a_named_cut(text: &str, findings: &[Finding]) -> Option<Kind> {
    let kind = findings
        .iter()
        .rev()
        .map(|finding| &finding.kind)
        .find(|kind| **kind != Kind::HighEntropy)?;
    text.trim_end()
        .ends_with(&scan::marker(kind))
        .then(|| kind.clone())
}

/// Give every tool call in the outgoing copy an id this program wrote.
///
/// **An id was the one field held to a weaker rule than everything
/// else.** It has to travel byte for byte, because it is what matches a
/// result to its call, so it could not carry a marker — and the entropy
/// rule could not be consulted about it either, since an id is an opaque
/// blob by construction and that is exactly what the entropy rule cuts.
/// So it was held to the *named* shapes only, and a secret with no named
/// shape went through: measured, a forty-four character opaque string
/// that layer 2 cuts out of prose reached the transport untouched inside
/// `ToolCall::id`, and so did `toolu_01_michael_furtak_1981_04_02_krakow`.
/// Ollama builds an id out of whatever the local model called the tool,
/// which is how a string the model wrote reaches this field.
///
/// **So the field stops carrying anything.** The endpoint needs an id to
/// be unique within the request and to match between a call and its
/// result; it does not need it to be any particular id. Numbering them
/// in order of appearance satisfies both, and it means there is no rule
/// to get right: whatever was in that field is not what leaves, because
/// what leaves is a counter.
///
/// It is done to the copy that is about to be encoded and never to the
/// conversation, so the agent goes on matching its own results to its
/// own calls by the ids it actually issued. The numbering is stable
/// while the prefix of the conversation is — a turn appends, so an
/// earlier call keeps its number, and prompt caching is undisturbed.
fn local_identifiers(request: &mut Request) {
    let mut known: Vec<(String, String)> = Vec::new();
    let mut rename = |id: &mut String| {
        if let Some((_, local)) = known.iter().find(|(was, _)| was == id) {
            *id = local.clone();
            return;
        }
        // A result whose call is not in the conversation still gets a
        // name of its own: an unmatched result is the endpoint's
        // complaint to make, and inventing a match here would be this
        // module editing the conversation to make it look well-formed.
        // The counter wears the shape the endpoint has always been sent.
        // Uniqueness and matching are the only two things the id has to
        // do, and a bare `call_0001` satisfies both — but "satisfies both"
        // is a claim about a remote validator nobody here has asked, and
        // the tests deliberately touch no network, so it would ship
        // unverified. Keeping the familiar prefix costs six characters and
        // removes the question: whatever was in the field still does not
        // leave, because what leaves is a counter.
        let local = format!("toolu_{:04}", known.len() + 1);
        known.push((std::mem::replace(id, local.clone()), local));
    };

    for message in &mut request.messages {
        for content in &mut message.content {
            match content {
                Content::ToolUse(call) => rename(&mut call.id),
                Content::ToolResult { id, .. } => rename(id),
                Content::Text(_) | Content::Thinking { .. } => {}
            }
        }
    }
}

/// Tell the far model that it is reading a payload with holes in it.
///
/// In the system prompt because that is the one place a request always
/// has room for a sentence: a message list has rules about what may
/// follow what, and a note appended to the wrong one is a 400. The cost
/// is a cache miss on the turns where something was actually removed,
/// which is the cheaper half of the trade — a model that reads a marker
/// without this note treats it as noise and answers as though nothing
/// was missing.
fn say_something_was_withheld(request: &mut Request) {
    match &mut request.system {
        Some(system) if system.is_empty() => *system = WITHHELD.to_string(),
        Some(system) => {
            system.push_str("\n\n");
            system.push_str(WITHHELD);
        }
        None => request.system = Some(WITHHELD.to_string()),
    }
}

/// Everything a request would carry off this machine, as one payload.
///
/// This is the definition of the number on a manifest: the size the user
/// is shown is the length of this string, and so is
/// [`Sealed::bytes`]. It is also what layer 3 is given to read, because
/// meaning does not stop at a message boundary.
///
/// Public because an interface may want to say how much a turn would
/// send before offering to send it, and because a second implementation
/// of "what counts as text in a request" is exactly the drift this
/// function exists to prevent.
///
/// The pieces are joined the way [`Gathering`](crate::redact::Gathering)
/// joins the pieces of a text payload, so a size means the same thing
/// whichever road it came down.
///
/// It walks a copy, which is the price of there being only one walk:
/// [`each_text`] hands out `&mut String` so that the layers can edit in
/// place, and a second read-only walk beside it would be a second list
/// of the fields that carry text — which is how one of them ends up
/// short a field nobody noticed.
pub fn payload_of(request: &Request) -> String {
    let mut copy = request.clone();
    let mut pieces: Vec<String> = Vec::new();
    // Every slot, not only the editable ones. The manifest's promise is
    // that nothing reaches a socket this number did not include, and a
    // string that may not be edited still reaches one: with a four
    // thousand character signature in the request the manifest read
    // "12 bytes" and the body was 4252. Counting is not editing.
    each_string(&mut copy, &mut |_, text| pieces.push(std::mem::take(text)));
    pieces.join("\n\n")
}

/// What a string in a request is for, and therefore what may be done to
/// it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    /// Prose. It can hold a marker, so layer 2 cuts it and layer 3 may
    /// cut more.
    Prose,
    /// A string the far side matches on or verifies. A marker in one of
    /// these does not make the request safer, it makes it a 400 — so it
    /// is read and counted and never edited, and anything found in it
    /// stops the turn instead.
    Verbatim,
}

/// Every string in a request that leaves this machine, in the order the
/// endpoint reads them, each with what may be done to it.
///
/// **There is one of these walks and it lists every field.** That is the
/// whole point of the `Slot` beside each string rather than a second
/// function for the fields nobody may edit: a second walk is a second
/// list, and the day the two lists disagree is the day a field is on the
/// wire that nothing looked at. Measured, before this: a thinking
/// block's `signature` was in no walk at all, and the Anthropic encoder
/// wrote it out verbatim — a full `sk-ant-` key reached the transport
/// with no marker, no manifest entry and no chance for layer 3. The
/// module header claimed the walk was over the `Request` so that "an
/// encoder that starts sending a field it used to drop cannot open a
/// hole that nobody notices". It was over most of the `Request`.
fn each_string(request: &mut Request, f: &mut dyn FnMut(Slot, &mut String)) {
    // A published model id, and a marker in it is a 404 rather than a
    // safer request — so it is `Verbatim` rather than absent. It comes
    // off the command line, and a command line is a place a user can
    // paste anything.
    f(Slot::Verbatim, &mut request.model);

    if let Some(system) = &mut request.system {
        f(Slot::Prose, system);
    }

    for message in &mut request.messages {
        for content in &mut message.content {
            match content {
                Content::Text(text) => f(Slot::Prose, text),
                // Scanned even though the Anthropic encoder drops a
                // thinking block that has no signature: what is sealed
                // is the request, not the subset of it one encoder
                // happens to emit today.
                Content::Thinking { text, signature } => {
                    f(Slot::Prose, text);
                    // The provider mints it and the provider verifies
                    // it, so it cannot be edited — but it is a string on
                    // the wire, and `Content` is a public type whose
                    // fields anybody may fill in.
                    if let Some(signature) = signature {
                        f(Slot::Verbatim, signature);
                    }
                }
                Content::ToolUse(call) => strings_in(&mut call.input, &mut |text| {
                    f(Slot::Prose, text)
                }),
                Content::ToolResult { output, .. } => f(Slot::Prose, output),
            }
        }
    }

    for tool in &mut request.tools {
        f(Slot::Prose, &mut tool.description);
        strings_in(&mut tool.input_schema, &mut |text| f(Slot::Prose, text));
    }
}

/// The strings layer 2 and layer 3 may edit.
fn each_text(request: &mut Request, f: &mut dyn FnMut(&mut String)) {
    each_string(request, &mut |slot, text| {
        if slot == Slot::Prose {
            f(text);
        }
    });
}

/// Every string inside a JSON document: the values, never the keys.
///
/// A key is an argument's name, and the far side reads a call by it. A
/// marker in place of `"path"` is a tool call that cannot be run, so
/// redaction has nothing to offer here — and rewriting keys is worse
/// than useless, because two of them redacted to the same marker would
/// collapse into one and silently change what the call says rather than
/// stopping it.
///
/// Keys are not therefore unexamined. They are examined by
/// [`unsendable`], which reads them with layer 2 and stops the turn on a
/// hit instead of marking them, and which knows which of them this
/// program declared and which of them a model made up.
fn strings_in(value: &mut Value, f: &mut dyn FnMut(&mut String)) {
    match value {
        Value::String(text) => f(text),
        Value::Array(items) => {
            for item in items {
                strings_in(item, f);
            }
        }
        Value::Object(map) => {
            for entry in map.values_mut() {
                strings_in(entry, f);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

// ------------------------------ the strings that have to travel verbatim

/// Layer 2 over the strings a tool call cannot carry a marker in, and
/// the sentence the agent says when one of them turns out to have a
/// credential in it.
///
/// `None` means there was nothing to say. The reasoning behind refusing
/// rather than redacting here, and behind refusing on a shape rather
/// than on an unknown name, is in the module header — it is the whole
/// reason this function exists beside [`each_text`] rather than inside
/// it.
fn unsendable(request: &Request) -> Option<String> {
    let vocabulary = Vocabulary::of(request);

    if let Some(kind) = shape_in(&request.model) {
        return Some(will_not_send(
            "The model id this turn would ask for",
            "A model id travels to Claude exactly as it is, because it is what the endpoint \
             looks up.",
            &kind,
        ));
    }

    // The registry itself, which is the trust root of everything below:
    // a poisoned registry defeats this check by definition. Read anyway,
    // because a credential inside a declared name is still a credential
    // about to leave, and one pass over a dozen short strings is not a
    // cost worth reasoning about.
    for tool in &request.tools {
        if let Some(kind) = shape_in(&tool.name) {
            return Some(will_not_send(
                "One of the tools I declare",
                "A declaration's name travels to Claude exactly as it is, because it is \
                 what the far model dispatches on.",
                &kind,
            ));
        }
        // A declaration's own property names. `strings_in` walks values,
        // so a key in a schema was read by nothing at all, and the
        // encoder writes `input_schema` out raw — measured, a key put a
        // whole `sk-ant-` key on the wire. The asymmetry was introduced
        // by the patch that started checking a CALL's argument names and
        // stopped there.
        if let Some(kind) = shape_in_keys(&tool.input_schema, None) {
            return Some(will_not_send(
                "A property name in one of the schemas I declare",
                "A schema travels to Claude exactly as it is, because it is what tells the \
                 far model how to call the tool.",
                &kind,
            ));
        }
    }

    for message in &request.messages {
        for content in &message.content {
            match content {
                Content::ToolUse(call) => {
                    // `None` here is a call to a tool this request never
                    // offered, and it is also what makes every key below
                    // undeclared: nothing named it, so nothing reads it.
                    let arguments = vocabulary.arguments(&call.name);

                    if arguments.is_none() {
                        if let Some(kind) = shape_in(&call.name) {
                            return Some(will_not_send(
                                "A tool call in this conversation",
                                "It names no tool I have, so it could not have run here \
                                 either — and a name I did not declare is a string the \
                                 model wrote from one end to the other.",
                                &kind,
                            ));
                        }
                    }

                    if let Some(kind) = shape_in_keys(&call.input, arguments) {
                        return Some(will_not_send(
                            "An argument name in a tool call in this conversation",
                            "No schema I declared names it, so no tool of mine would have \
                             read it — and argument names travel to Claude exactly as \
                             they are.",
                            &kind,
                        ));
                    }
                }
                // A signature the provider minted is verified by the
                // provider, so a marker in it would cost the whole turn.
                // It is checked instead, and against the NAMED shapes
                // only: a genuine signature is a long opaque blob and
                // the entropy rule would refuse every signed turn there
                // has ever been.
                Content::Thinking {
                    signature: Some(signature),
                    ..
                } => {
                    if let Some(kind) = shape_in_named(signature) {
                        return Some(will_not_send(
                            "A thinking block's signature in this conversation",
                            "A signature travels to Claude exactly as it is, because the \
                             endpoint verifies it and rejects a block that was edited.",
                            &kind,
                        ));
                    }
                }
                // Ids are neither checked nor redacted: they are
                // rewritten. See [`local_identifiers`].
                Content::ToolResult { .. } | Content::Text(_) | Content::Thinking { .. } => {}
            }
        }
    }

    None
}

/// The names this program wrote, as the seal can see them.
///
/// Read off the request's own tool declarations, which are the registry
/// as it stood when the turn was built. Nothing here has to be handed
/// the agent's state, so nothing here can be looking at a registry the
/// agent has since changed.
struct Vocabulary<'a> {
    /// One entry per declared tool: its name, and every argument name
    /// anywhere in its schema. A `Vec` because a registry is a dozen
    /// tools, and a map would be a slower way to walk a dozen strings.
    tools: Vec<(&'a str, BTreeSet<&'a str>)>,
}

impl<'a> Vocabulary<'a> {
    fn of(request: &'a Request) -> Self {
        let mut tools = Vec::with_capacity(request.tools.len());
        for tool in &request.tools {
            let mut arguments = BTreeSet::new();
            argument_names(&tool.input_schema, &mut arguments);
            tools.push((tool.name.as_str(), arguments));
        }
        Vocabulary { tools }
    }

    /// What `tool` may call its arguments, or `None` when no such tool
    /// was declared.
    fn arguments(&self, tool: &str) -> Option<&BTreeSet<&'a str>> {
        self.tools
            .iter()
            .find(|(name, _)| *name == tool)
            .map(|(_, arguments)| arguments)
    }
}

/// Every argument name a JSON Schema mentions, at any depth.
///
/// Flattened on purpose. The question being asked is not "may this
/// argument appear here", which is the tool's own business when it runs
/// and which this module has no need of an opinion about — it is "did
/// this program write this string". A name under any `properties` of a
/// schema this program declared is one this program wrote, wherever in
/// the schema it sits.
///
/// A schema that names no keys at all — a free-form map — leaves the set
/// empty, and every key in the call is then read by layer 2. That costs
/// such a tool nothing, because a key that is genuinely a key comes back
/// clean.
fn argument_names<'a>(schema: &'a Value, into: &mut BTreeSet<&'a str>) {
    match schema {
        Value::Object(map) => {
            for (keyword, value) in map {
                match (keyword.as_str(), value) {
                    ("properties", Value::Object(fields)) => {
                        for (field, sub) in fields {
                            into.insert(field.as_str());
                            argument_names(sub, into);
                        }
                    }
                    ("required", Value::Array(names)) => {
                        into.extend(names.iter().filter_map(Value::as_str));
                    }
                    _ => argument_names(value, into),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                argument_names(item, into);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// Every key in a tool call's arguments that `declared` does not name,
/// read by layer 2, and the first thing it found.
fn shape_in_keys(value: &Value, declared: Option<&BTreeSet<&str>>) -> Option<Kind> {
    match value {
        Value::Object(map) => map.iter().find_map(|(key, entry)| {
            let named = declared.is_some_and(|names| names.contains(key.as_str()));
            if !named {
                if let Some(kind) = shape_in(key) {
                    return Some(kind);
                }
            }
            shape_in_keys(entry, declared)
        }),
        Value::Array(items) => items.iter().find_map(|item| shape_in_keys(item, declared)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
    }
}

/// What layer 2 finds in a string, or `None`.
///
/// The redacted text is thrown away. What is wanted here is whether
/// there was anything and what kind of thing it was — nothing this is
/// asked about is going to be sent, marked or otherwise.
fn shape_in(text: &str) -> Option<Kind> {
    scan::scan(text)
        .findings
        .into_iter()
        .next()
        .map(|finding| finding.kind)
}

/// The same, for a string that must travel byte for byte and that the
/// entropy rule may therefore not speak about.
///
/// A provider's own signature is a long opaque blob with no
/// natural-language profile — which is exactly what the entropy rule
/// looks for — so consulting it here would refuse every signed turn
/// there has ever been. It is held to the *named* shapes instead: a
/// provider prefix, an armour line, a JWT, a labelled value. No provider
/// mints a signature that looks like any of those.
///
/// Every finding is looked at rather than only the first, because a
/// string with a key stuck on the end of an opaque prefix has the
/// harmless finding first.
fn shape_in_named(text: &str) -> Option<Kind> {
    scan::scan(text)
        .findings
        .into_iter()
        .map(|finding| finding.kind)
        .find(|kind| *kind != Kind::HighEntropy)
}

/// The refusal, which names the kind of thing that was found and never
/// the thing itself.
///
/// It opens with [`escalate::CANNOT`](super::escalate) for the same
/// reason every other refusal does: a pinned session, an unreachable
/// provider and a payload that may not go all leave the agent in one
/// place, and three different sentences would teach the user that two of
/// them are less serious than the third.
fn will_not_send(subject: &str, travels: &str, kind: &Kind) -> String {
    format!(
        "{} {subject} has {} in it. {travels} I have not sent the turn and I have not \
         guessed at what it was meant to say — nothing left this machine.",
        super::escalate::CANNOT,
        kind.what(),
    )
}
