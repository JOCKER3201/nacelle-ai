//! When the local agent asks for help, and when it is told it may not.
//!
//! The decision to escalate must not rest on the local model saying "I
//! can't do this". Small models are unreliable narrators of their own
//! competence in both directions: they give up on work they could have
//! done, and they finish work they mangled without noticing. An
//! escalation path built on that one signal escalates constantly, or
//! never, and in both cases for reasons nobody can inspect.
//!
//! So the triggers here are mostly things a counter can see. The model's
//! own request is one trigger among several, it has to come with a
//! reason the user can read, and it is worth no more than the others.
//!
//! | trigger | who noticed |
//! |---|---|
//! | [`Trigger::UserAsked`] | the person; always a valid reason, never second-guessed |
//! | [`Trigger::RepeatedFailure`] | a counter — see [`Attempts`] |
//! | [`Trigger::ContextExceeded`] | arithmetic |
//! | [`Trigger::MissingCapability`] | the loaded model's declared abilities |
//! | [`Trigger::ModelAsked`] | the model, with its reason attached |
//!
//! ## Refusing to escalate
//!
//! A session can be pinned local, and then the agent says what it cannot
//! do instead of reaching for the network. The two ways this happens by
//! accident — no credential, no route to the provider — degrade to
//! **exactly the same behaviour**, which is the property this module is
//! built around: the local half must never depend on the remote half
//! being reachable, so "I am pinned", "I have no token" and "I cannot
//! reach it" are one code path with three explanations, not three
//! different failure modes discovered at three different times.
//!
//! ## The user's explicit request, and the pin
//!
//! These can contradict each other: the session is pinned, and the user
//! then says "ask Claude". The pin wins, and the agent says so and says
//! how to lift it. That is not the request being ignored — it is the
//! same person's earlier and more deliberate instruction being honoured
//! until they take it back, and taking it back is one call to
//! [`Policy::unpin`]. The alternative, a request that silently overrides
//! the pin, would make the pin a suggestion, and a suggestion is not
//! what anybody pins a session for.

use std::collections::BTreeMap;
use std::fmt;

use crate::error::BackendError;

/// Why the local agent would hand this to the remote one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// The person asked, in as many words. Always available, always
    /// honoured as a reason — no heuristic here gets to decide the user
    /// did not mean it.
    UserAsked,
    /// The same piece of work failed more than once. Counted, not
    /// judged: see [`Attempts`].
    RepeatedFailure { task: String, attempts: u32 },
    /// The work does not fit in the local model's context window.
    ContextExceeded { needed: usize, window: usize },
    /// The local model cannot do this kind of thing at all — no tool
    /// support in what is loaded, no vision, no long output.
    MissingCapability { needed: String },
    /// The model asked, and said why. There is no way to build this
    /// without a reason; see [`Trigger::model_asked`].
    ModelAsked { reason: String },
}

impl Trigger {
    /// The model's own request, or nothing.
    ///
    /// A blank reason is not a request. "I think we should escalate" is
    /// a sentence the user cannot evaluate, and an escalation the user
    /// cannot evaluate is one they will approve out of habit.
    pub fn model_asked(reason: &str) -> Option<Trigger> {
        let reason = reason.trim();
        (!reason.is_empty()).then(|| Trigger::ModelAsked {
            reason: reason.to_string(),
        })
    }

    /// The work does not fit, if it does not.
    pub fn context_exceeded(needed: usize, window: usize) -> Option<Trigger> {
        (needed > window).then_some(Trigger::ContextExceeded { needed, window })
    }

    /// One line for the manifest: why this would be sent.
    pub fn reason(&self) -> String {
        match self {
            Trigger::UserAsked => "you asked for this to go to Claude".to_string(),
            Trigger::RepeatedFailure { task, attempts } => {
                format!("the local model failed \"{task}\" {attempts} times")
            }
            Trigger::ContextExceeded { needed, window } => format!(
                "the work is about {needed} bytes and the local model's context holds {window}"
            ),
            Trigger::MissingCapability { needed } => {
                format!("the local model has no {needed}")
            }
            Trigger::ModelAsked { reason } => {
                format!("the local model asked to escalate: {reason}")
            }
        }
    }

    /// What could not be done here, for the sentence the user is told
    /// when escalation is refused.
    pub fn shortfall(&self) -> String {
        match self {
            Trigger::UserAsked => "you asked me to hand this to Claude".to_string(),
            Trigger::RepeatedFailure { task, .. } => {
                format!("I could not get \"{task}\" right on my own")
            }
            Trigger::ContextExceeded { needed, window } => format!(
                "this is about {needed} bytes and I can only hold {window} at a time"
            ),
            Trigger::MissingCapability { needed } => {
                format!("I have no {needed}, so I cannot do this part at all")
            }
            Trigger::ModelAsked { reason } => reason.clone(),
        }
    }
}

impl fmt::Display for Trigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason())
    }
}

/// What the remote half looks like from here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Remote {
    /// A credential was found and the provider has not said otherwise.
    Ready,
    /// No credential. Carries the message from
    /// [`credentials`](crate::credentials), which already says where to
    /// put one.
    NoCredential(String),
    /// The last attempt did not get there: DNS, no route, a refused
    /// connection.
    Unreachable(String),
}

impl Remote {
    /// What one failed turn says about the remote half, when it says
    /// anything.
    ///
    /// This exists so that "no network degrades exactly like a pin" is
    /// something the code falls into rather than something a caller has
    /// to remember to arrange. A caller that hands every failure to
    /// [`Policy::observe`] cannot end up in the state this module is
    /// written to prevent: reaching for a provider that has already
    /// stopped answering, once per turn, for the rest of the session.
    ///
    /// `None` for everything else. A rate limit, a refusal and an
    /// unreadable reply are all answers, and an answer means the remote
    /// half is plainly there — treating them as unreachable would pin a
    /// session over one bad turn.
    ///
    /// A credential the provider REJECTED lands on
    /// [`Remote::NoCredential`] with the rest. The two are not the same
    /// thing, but they are the same thing to do about it: this machine
    /// has no credential that works, and the user has to supply one.
    pub fn from_error(error: &BackendError) -> Option<Remote> {
        match error {
            BackendError::Network(message) => Some(Remote::Unreachable(message.clone())),
            BackendError::Auth(message) => Some(Remote::NoCredential(message.clone())),
            BackendError::RateLimited { .. }
            | BackendError::Refused { .. }
            | BackendError::Protocol(_)
            | BackendError::Server { .. }
            // This machine's own decision, arriving back as an error.
            // Recording it as a fact about the remote half would let a
            // refusal teach the policy that the network is down.
            | BackendError::Withheld(_)
            | BackendError::Cancelled => None,
        }
    }
}

/// Why an escalation is not going to happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grounds {
    /// The user pinned this session to the local model.
    PinnedLocal,
    NoCredential,
    Unreachable,
}

impl Grounds {
    /// The clause after "I am not asking Claude".
    pub fn because(&self) -> &'static str {
        match self {
            Grounds::PinnedLocal => "this session is pinned to the local model",
            Grounds::NoCredential => "there is no credential for it on this machine",
            Grounds::Unreachable => "it cannot be reached from here",
        }
    }

    /// What the user can do about it, when there is something.
    pub fn remedy(&self) -> &'static str {
        match self {
            Grounds::PinnedLocal => "Unpin the session if you want me to ask.",
            Grounds::NoCredential => {
                "Set ANTHROPIC_AUTH_TOKEN or write the credentials file if you want me to ask."
            }
            Grounds::Unreachable => "Check the network if you want me to try again.",
        }
    }
}

/// The first sentence of every refusal, whatever the grounds.
///
/// Fixed text on purpose. A machine with no token, a machine with no
/// network and a session the user pinned all leave the agent in the same
/// position, and describing that position three different ways would
/// teach the user that two of them are worse than the third.
pub(crate) const CANNOT: &str = "I am not going to ask Claude about this.";

/// What to do about one trigger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Escalation is allowed. It still does not happen until the user
    /// has seen a manifest and said so — this is permission to ask them,
    /// not permission to send.
    Ask { trigger: Trigger },
    /// Escalation is refused. `tell` is what the agent says out loud,
    /// which always names what it could not do here.
    Stay {
        trigger: Trigger,
        grounds: Grounds,
        tell: String,
    },
}

impl Decision {
    pub fn is_ask(&self) -> bool {
        matches!(self, Decision::Ask { .. })
    }

    pub fn trigger(&self) -> &Trigger {
        match self {
            Decision::Ask { trigger } | Decision::Stay { trigger, .. } => trigger,
        }
    }
}

/// Whether this session may reach the network, and why not when it may
/// not.
#[derive(Clone, Debug)]
pub struct Policy {
    pinned: bool,
    remote: Remote,
}

impl Policy {
    /// A session that may escalate once the remote half is usable.
    pub fn new(remote: Remote) -> Self {
        Policy {
            pinned: false,
            remote,
        }
    }

    /// A session that will not escalate whatever happens.
    ///
    /// The remote state is still recorded, so the reason the user is
    /// given is the pin — which is the one they can act on — rather than
    /// a missing token they never asked about.
    pub fn local_only() -> Self {
        Policy {
            pinned: true,
            remote: Remote::Ready,
        }
    }

    pub fn pin(&mut self) {
        self.pinned = true;
    }

    pub fn unpin(&mut self) {
        self.pinned = false;
    }

    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Record what the remote half looks like now — a credential
    /// resolved, a request that failed to connect.
    pub fn set_remote(&mut self, remote: Remote) {
        self.remote = remote;
    }

    /// Learn from a turn that failed.
    ///
    /// Failures that say nothing about reachability are ignored — see
    /// [`Remote::from_error`]. What this records is deliberately sticky:
    /// coming back from [`Remote::Unreachable`] is `set_remote(Remote::
    /// Ready)`, an act rather than a timeout, because a supervisor that
    /// quietly re-armed itself would go back to reaching for a network
    /// that is still not there and the user would learn about it one
    /// stalled turn at a time.
    pub fn observe(&mut self, error: &BackendError) {
        if let Some(remote) = Remote::from_error(error) {
            self.remote = remote;
        }
    }

    pub fn remote(&self) -> &Remote {
        &self.remote
    }

    /// Why escalation would be refused right now, or nothing.
    ///
    /// The pin is checked first: it is the user's own decision, and
    /// telling them about a missing token when they asked for a session
    /// that would not use one would be answering a question nobody put.
    pub fn blocked(&self) -> Option<Grounds> {
        if self.pinned {
            return Some(Grounds::PinnedLocal);
        }
        match &self.remote {
            Remote::Ready => None,
            Remote::NoCredential(_) => Some(Grounds::NoCredential),
            Remote::Unreachable(_) => Some(Grounds::Unreachable),
        }
    }

    /// One line an interface can show: whether this session can escalate
    /// at all.
    pub fn status(&self) -> String {
        match self.blocked() {
            None => "local model, with Claude available when it is needed".to_string(),
            Some(grounds) => format!("local model only — {}", grounds.because()),
        }
    }

    /// What to do about a trigger.
    pub fn decide(&self, trigger: Trigger) -> Decision {
        match self.blocked() {
            None => Decision::Ask { trigger },
            Some(grounds) => {
                let tell = format!(
                    "{} {CANNOT} Why not: {}. {}",
                    sentence(&trigger.shortfall()),
                    grounds.because(),
                    grounds.remedy()
                );
                Decision::Stay {
                    trigger,
                    grounds,
                    tell,
                }
            }
        }
    }
}

/// A leading clause turned into a sentence.
///
/// Shared with [`handoff`](super::handoff) so that a refusal by the
/// policy and a refusal by the user open the same way: with what could
/// not be done here.
pub(super) fn sentence(clause: &str) -> String {
    let clause = clause.trim();
    let mut out = String::with_capacity(clause.len() + 1);
    let mut chars = clause.chars();
    if let Some(first) = chars.next() {
        out.extend(first.to_uppercase());
        out.push_str(chars.as_str());
    }
    if !out.ends_with('.') && !out.ends_with('!') && !out.ends_with('?') {
        out.push('.');
    }
    out
}

/// How many times each piece of work has failed.
///
/// The deterministic half of "the local model failed the same task
/// twice". What counts as the same task is the caller's key — a tool
/// name, a normalised question — and it is a string rather than an
/// identifier so that an interface can key it on whatever it actually
/// has.
#[derive(Clone, Debug)]
pub struct Attempts {
    limit: u32,
    seen: BTreeMap<String, u32>,
}

impl Default for Attempts {
    fn default() -> Self {
        Attempts::new()
    }
}

impl Attempts {
    /// Two: one failure is a mistake and two is a pattern. A higher
    /// number spends the user's time watching the same thing fail; a
    /// lower one escalates work the local model would have finished on
    /// the retry.
    pub const DEFAULT_LIMIT: u32 = 2;

    pub fn new() -> Self {
        Attempts::with_limit(Attempts::DEFAULT_LIMIT)
    }

    pub fn with_limit(limit: u32) -> Self {
        Attempts {
            limit: limit.max(1),
            seen: BTreeMap::new(),
        }
    }

    /// Record a failure, and say whether it is now a reason to escalate.
    pub fn failed(&mut self, task: &str) -> Option<Trigger> {
        let count = self.seen.entry(task.to_string()).or_insert(0);
        *count += 1;
        (*count >= self.limit).then(|| Trigger::RepeatedFailure {
            task: task.to_string(),
            attempts: *count,
        })
    }

    /// Record a success. The count goes, so a task that failed once an
    /// hour ago and worked since does not escalate the next time it
    /// stumbles.
    pub fn succeeded(&mut self, task: &str) {
        self.seen.remove(task);
    }

    pub fn count(&self, task: &str) -> u32 {
        self.seen.get(task).copied().unwrap_or(0)
    }

    /// Forget everything — a new exchange, or a session reset.
    pub fn clear(&mut self) {
        self.seen.clear();
    }
}
