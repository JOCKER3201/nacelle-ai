//! Who says yes: the decision that stands between the model asking for
//! a change and the change happening.
//!
//! The rule this module exists to keep is that **the agent never
//! changes anything on its own**. A tool the registry called
//! [`Effect::Change`](super::registry::Effect::Change) does not run
//! until somebody outside the loop has said so, for that call, this
//! time. There is no session-wide grant and no remembered answer: the
//! cost of asking again is a click, and the cost of a forgotten blanket
//! yes is whatever the model does next.
//!
//! A refusal is not a failure of the turn. It goes back to the model as
//! the tool's result, saying the user declined and why, so the model
//! learns that this route is closed and proposes another one. An agent
//! that was merely told "error" would try the same call again; one that
//! is told nothing at all would report success it never had.
//!
//! Nothing here decides anything. The interface does — a dialog in the
//! desktop, a prompt in a terminal, a channel back to whichever thread
//! owns the screen. What this module fixes is the shape of the
//! question and of the answer.

use crate::message::ToolCall;

use super::registry::Change;

/// What the interface answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Run it, this once.
    Allow,
    /// Do not run it. The reason reaches the model as the tool result,
    /// so word it as something the model can act on — "not that
    /// directory", "ask me again after the demo" — rather than as a
    /// status code.
    Deny { reason: Option<String> },
    /// Do not run it, and stop the whole exchange. The user is not
    /// declining one tool, they are done.
    Cancel,
}

impl Decision {
    pub fn deny(reason: impl Into<String>) -> Self {
        Decision::Deny {
            reason: Some(reason.into()),
        }
    }

    /// Declined without a reason given. The model still learns that the
    /// user said no, which is the part that changes its plan.
    pub fn denied() -> Self {
        Decision::Deny { reason: None }
    }
}

/// What the interface is being asked about.
///
/// Borrowed rather than owned: the loop already holds both, and an
/// approver that only shows a dialog should not have to pay for copies
/// of the arguments to do it.
#[derive(Clone, Copy, Debug)]
pub struct ApprovalRequest<'a> {
    /// The call as the model made it, arguments and all. Whatever is
    /// shown to the user comes from here — the change summary says what
    /// would happen, this says exactly what was asked for.
    pub call: &'a ToolCall,
    /// What the registry says the call would change.
    pub change: &'a Change,
}

/// Whoever answers for the user.
///
/// Implemented for any `FnMut`, so a caller that just wants a rule —
/// a terminal prompt, a test that always says no — writes a closure
/// and a caller that needs state writes a type.
pub trait Approver {
    fn approve(&mut self, request: ApprovalRequest<'_>) -> Decision;
}

impl<F> Approver for F
where
    F: FnMut(ApprovalRequest<'_>) -> Decision,
{
    fn approve(&mut self, request: ApprovalRequest<'_>) -> Decision {
        self(request)
    }
}

/// Says no to everything.
///
/// The right approver when there is nobody to ask: a headless run, a
/// widget whose window has gone away, a session the user pinned to
/// read-only. It is also the only stock approver in this crate — an
/// `AllowAll` would be a one-line way to lose the guarantee the rest of
/// this module is for, and anyone who genuinely wants one can write the
/// closure and own that decision.
pub struct DenyAll;

impl Approver for DenyAll {
    fn approve(&mut self, _request: ApprovalRequest<'_>) -> Decision {
        Decision::Deny {
            reason: Some("this agent is running with nobody to ask, so it cannot change anything".to_string()),
        }
    }
}
