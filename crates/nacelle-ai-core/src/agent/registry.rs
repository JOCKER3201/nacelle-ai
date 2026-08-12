//! The tools the agent may use, as the loop sees them.
//!
//! The loop knows nothing about what any tool does. It knows four
//! things, and this trait is where it asks for them: which tools exist,
//! what the machine they act on looks like, whether running one would
//! change anything, and what happened when it ran.
//!
//! That split is deliberate. [`ToolRegistry::effect`] is asked *before*
//! [`ToolRegistry::invoke`] and cannot run anything, so the decision
//! "does the user have to approve this" is taken by a call that has no
//! way to have already acted. A registry that answered
//! [`Effect::Read`] for a tool that writes would defeat the approval
//! path entirely, which is why that answer lives with the tools rather
//! than with the loop guessing from a name.
//!
//! [`ToolRegistry::environment`] is what the agent is told about the
//! desktop it manages — which themes, layouts and addons are installed.
//! It is asked once, when a session starts, and never again during it:
//! the system prompt has to stay byte-identical from turn to turn for
//! the provider to cache it, and a prompt rebuilt on every turn would
//! silently cost the full price of the prefix each time.

use crate::message::{ToolCall, ToolDeclaration};

/// One thing the agent should know about the machine it manages.
///
/// Facts are rendered into the system prompt in the order the registry
/// returns them, and that order is part of the contract: a registry
/// that builds this list by iterating a `HashMap` produces a different
/// prompt on every process start, which reads the same to a human and
/// misses the provider's cache every time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvironmentFact {
    /// What this is about — `"themes"`, `"layouts"`, `"addons"`.
    pub topic: String,
    /// A sentence of context, when the topic needs one.
    pub note: Option<String>,
    /// What is available, one entry per thing.
    pub items: Vec<String>,
}

impl EnvironmentFact {
    pub fn new(topic: impl Into<String>) -> Self {
        EnvironmentFact {
            topic: topic.into(),
            note: None,
            items: Vec::new(),
        }
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    pub fn with_items<I, S>(mut self, items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.items = items.into_iter().map(Into::into).collect();
        self
    }
}

/// What running a tool would do to the machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Only reads. The loop runs it without asking anyone.
    Read,
    /// Changes something. The loop stops and puts it to the user
    /// first, showing this description of the change.
    Change(Change),
}

/// A change, in the words the user is shown before it happens.
///
/// The user is being asked to authorise something they did not type, so
/// the summary has to be enough to answer with: what is being changed
/// and where. `detail` carries the specifics — the path, the old value
/// and the new one — for an interface that can show more than a line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Change {
    /// One line: `"set the theme to amber"`.
    pub summary: String,
    /// The specifics, when there are any worth showing.
    pub detail: Option<String>,
}

impl Change {
    pub fn new(summary: impl Into<String>) -> Self {
        Change {
            summary: summary.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

/// What a tool answered.
///
/// A failure is a result, not an error return: the model asked for
/// something, and being told plainly that it did not work is how it
/// tries something else. A tool that returned `Err` up the stack would
/// end the turn instead, leaving the model with no idea what happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolOutput {
    /// What the model reads. Written for the model, not for a log.
    pub output: String,
    /// Whether this says "it did not work". Providers mark such results
    /// so the model can tell a failure from an answer that happens to
    /// contain the word "error".
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(output: impl Into<String>) -> Self {
        ToolOutput {
            output: output.into(),
            is_error: false,
        }
    }

    pub fn error(output: impl Into<String>) -> Self {
        ToolOutput {
            output: output.into(),
            is_error: true,
        }
    }
}

/// Everything the agent loop needs to know about tools.
///
/// `Send` because the loop runs on a worker thread and the registry
/// goes with it. Not `Sync`: one turn at a time, and `invoke` takes
/// `&mut self` precisely so a registry may hold state a tool changes.
pub trait ToolRegistry: Send {
    /// The tools the model is told it may call.
    ///
    /// Read once per session, like [`ToolRegistry::environment`] and
    /// for the same reason: on most providers the tool declarations sit
    /// in the cached prefix next to the system prompt, so a list that
    /// changed between turns would throw that cache away.
    fn declarations(&self) -> Vec<ToolDeclaration>;

    /// What the agent should know about the machine, for the system
    /// prompt. Empty is a valid answer — an agent with no tools has
    /// nothing to describe.
    fn environment(&self) -> Vec<EnvironmentFact> {
        Vec::new()
    }

    /// What running `call` would do, asked before it runs.
    ///
    /// A call naming a tool that does not exist is [`Effect::Read`]:
    /// there is nothing to authorise, and [`ToolRegistry::invoke`] will
    /// tell the model so.
    fn effect(&self, call: &ToolCall) -> Effect;

    /// Run it.
    ///
    /// Never panics on a call the model got wrong — an unknown name,
    /// missing arguments, a path outside what the tool may touch. All
    /// of those are [`ToolOutput::error`], because the model is the one
    /// that has to correct them.
    fn invoke(&mut self, call: &ToolCall) -> ToolOutput;
}

/// A registry with nothing in it: the agent as a conversation and
/// nothing more.
///
/// Useful on its own — an agent that can only talk still answers
/// questions — and it is what the loop is tested against where the test
/// is about the loop rather than about a tool.
pub struct NoTools;

impl ToolRegistry for NoTools {
    fn declarations(&self) -> Vec<ToolDeclaration> {
        Vec::new()
    }

    fn effect(&self, _call: &ToolCall) -> Effect {
        Effect::Read
    }

    fn invoke(&mut self, call: &ToolCall) -> ToolOutput {
        // The model was told about no tools at all, so this is either a
        // provider echoing something odd or a model inventing a call.
        // Either way, saying so is better than a silent empty result.
        ToolOutput::error(format!(
            "there is no tool called {:?}: this agent has no tools",
            call.name
        ))
    }
}
