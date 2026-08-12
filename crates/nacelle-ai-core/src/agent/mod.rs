//! The loop: ask the model, run what it asked for, ask again, stop.
//!
//! | module | what it is |
//! |---|---|
//! | [`registry`] | the tools, as the loop sees them |
//! | [`approval`] | who says yes before anything changes |
//! | [`history`] | the conversation, and how it is cut down |
//! | [`prompt`] | the system prompt, built once per session |
//! | [`worker`] | the same loop, on a thread of its own |
//!
//! One turn of a conversation is rarely one request. The model answers,
//! asks for a tool, is given the result, and answers again — and that
//! cycle is the agent. What is worth writing down is where it stops and
//! what it refuses to do on the way.
//!
//! **It stops.** [`Limits::max_turns`] is a hard ceiling on how many
//! times the model may come back asking for another tool. A model that
//! loops — and they do, given a tool that keeps failing in the same way
//! — would otherwise spend the user's money until something else broke.
//! Hitting the ceiling is [`AgentError::TurnLimit`], said plainly,
//! rather than a quiet stop that reads like an answer.
//!
//! **It does not change anything by itself.** Every call the registry
//! marks as [`Effect::Change`] is put to an [`Approver`] first, and a
//! refusal goes back to the model as that tool's result. See
//! [`approval`].
//!
//! **It leaves the conversation valid.** Whatever happens — a refusal,
//! a cancellation half way through the tools, the turn ceiling — every
//! tool call the model made ends up with a result beside it. The
//! providers reject a conversation where one does not, so a history
//! that could not be sent again would turn one bad turn into a dead
//! session.
//!
//! **It does not own a thread.** [`Agent::ask`] blocks for as long as
//! the exchange takes, which is right for a worker and wrong for an
//! interface; [`worker::Worker`] is the same loop with a thread and a
//! channel around it.

pub mod approval;
pub mod history;
pub mod prompt;
pub mod registry;
pub mod worker;

use std::error::Error;
use std::fmt;

use crate::backend::{Backend, Flow};
use crate::error::BackendError;
use crate::event::{StopReason, StreamEvent, Usage};
use crate::message::{Content, Message, Request, Role, ToolCall, ToolDeclaration, DEFAULT_MAX_TOKENS};

pub use approval::{ApprovalRequest, Approver, Decision, DenyAll};
pub use history::History;
pub use registry::{Change, Effect, EnvironmentFact, NoTools, ToolOutput, ToolRegistry};
pub use worker::{PendingApproval, Worker};

/// Where the loop gives up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// How many times the model may answer in one exchange. Each tool
    /// round is a turn, so this is also the most tools-then-answer
    /// cycles a single question can cost.
    pub max_turns: u32,
    /// Roughly how many bytes of conversation to keep. See
    /// [`history`] for what "roughly" means and why it is bytes.
    pub history_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            // Enough for a real piece of work — read a few files,
            // change one, check the result — and short enough that a
            // model stuck in a loop is stopped while the user is still
            // watching rather than after they have walked away.
            max_turns: 16,
            // About fifty thousand tokens of prose, which leaves room
            // for the reply and for the tool results of the turn that
            // is about to happen inside a 200k context.
            history_bytes: 200_000,
        }
    }
}

/// What happens while the agent works.
///
/// [`Agent::ask`] emits everything up to and including the tool events;
/// the end of an exchange is its return value. [`Worker`] emits the
/// same events and then [`AgentEvent::Finished`] or
/// [`AgentEvent::Failed`], because a channel has no return value, and
/// [`AgentEvent::Approval`], because on a worker thread the question
/// has to travel to whoever owns the screen.
#[derive(Debug)]
pub enum AgentEvent {
    /// A model turn has begun. `turn` counts from one within this
    /// exchange, so an interface can say "still working (3/16)".
    TurnStarted { turn: u32, model: String },
    /// A fragment of the visible answer.
    Text(String),
    /// A fragment of the model's reasoning.
    Thinking(String),
    /// A tool is about to run: approved, or never needed approval.
    ToolStarted { call: ToolCall },
    ToolFinished {
        id: String,
        name: String,
        is_error: bool,
    },
    /// The user was asked and said no. Carries the sentence the model
    /// will be given, so the interface and the model tell the same
    /// story about what happened.
    ToolDenied {
        id: String,
        name: String,
        reason: String,
    },
    /// A change is waiting on the user. Only [`Worker`] produces this;
    /// answer it, or drop it — a dropped request counts as a refusal.
    Approval(PendingApproval),
    /// The exchange finished. Only [`Worker`] produces this.
    Finished(Completion),
    /// The exchange did not finish. Only [`Worker`] produces this.
    Failed(AgentError),
}

/// Where the interface's view of the exchange goes.
///
/// The same shape as [`EventSink`](crate::backend::EventSink), and for
/// the same reasons: returning [`Flow::Stop`] is how a reader cancels,
/// and pushing into a [`std::sync::mpsc::Sender`] needs no adapter.
pub type AgentSink<'a> = dyn FnMut(AgentEvent) -> Flow + 'a;

/// What an exchange came to.
#[derive(Clone, Debug, PartialEq)]
pub struct Completion {
    /// The model's last word: the text of the final turn, which is the
    /// answer. Anything it narrated on the way there went through the
    /// sink as it happened and is not repeated here.
    pub text: String,
    /// How many times the model answered.
    pub turns: u32,
    /// How many tools actually ran. Refused ones are not counted —
    /// they did not run.
    pub tools_run: u32,
    /// Every turn's usage, added up.
    pub usage: Usage,
    /// Why the last turn stopped.
    pub stop: StopReason,
}

/// Why an exchange did not finish.
#[derive(Clone, Debug, PartialEq)]
pub enum AgentError {
    /// The provider failed, or refused. Passed through whole: the
    /// caller decides about retrying, and [`BackendError::is_retryable`]
    /// is what it decides with.
    Backend(BackendError),
    /// The model was still asking for tools when the ceiling was
    /// reached. The conversation is intact and can be continued by
    /// asking again — this is a stop, not a corruption.
    TurnLimit { limit: u32 },
    /// The user stopped it: from the sink, from
    /// [`Worker::cancel`](worker::Worker::cancel), or by answering an
    /// approval with [`Decision::Cancel`].
    Cancelled,
}

impl From<BackendError> for AgentError {
    fn from(err: BackendError) -> Self {
        match err {
            // The user pressed stop; which layer noticed first is not
            // something they should have to read about.
            BackendError::Cancelled => AgentError::Cancelled,
            other => AgentError::Backend(other),
        }
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::Backend(err) => write!(f, "{err}"),
            AgentError::TurnLimit { limit } => write!(
                f,
                "the agent stopped after {limit} turns without finishing — it kept asking for tools"
            ),
            AgentError::Cancelled => write!(f, "stopped"),
        }
    }
}

impl Error for AgentError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            AgentError::Backend(err) => Some(err),
            _ => None,
        }
    }
}

/// One agent: a model, a set of tools, and the conversation between
/// them.
///
/// Holds everything that is true for a whole session — the system
/// prompt, the tool declarations, the history — and nothing about how
/// any of it is displayed. One agent is one conversation; a second
/// conversation is a second agent, or the same one after
/// [`Agent::reset`].
pub struct Agent {
    backend: Box<dyn Backend>,
    tools: Box<dyn ToolRegistry>,
    /// What the environment looked like when the session started. Kept
    /// so [`Agent::with_role`] can re-render the prompt without asking
    /// the registry again and getting a different answer mid-session.
    facts: Vec<EnvironmentFact>,
    declarations: Vec<ToolDeclaration>,
    role: Option<String>,
    system: String,
    model: String,
    max_tokens: u32,
    thinking: bool,
    limits: Limits,
    history: History,
}

impl Agent {
    /// Start a session.
    ///
    /// The registry is asked what it can do and what the machine looks
    /// like here, once, and the answers are fixed for the life of this
    /// agent — see [`prompt`] for why that matters to the bill.
    pub fn new(
        backend: Box<dyn Backend>,
        tools: Box<dyn ToolRegistry>,
        model: impl Into<String>,
    ) -> Self {
        let limits = Limits::default();
        let facts = tools.environment();
        let declarations = tools.declarations();
        let system = prompt::build(None, &facts);

        Agent {
            backend,
            tools,
            facts,
            declarations,
            role: None,
            system,
            model: model.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            thinking: false,
            limits,
            history: History::new(limits.history_bytes),
        }
    }

    /// Extra standing instructions, folded into the system prompt.
    pub fn with_role(mut self, role: impl Into<String>) -> Self {
        self.role = Some(role.into());
        self.system = prompt::build(self.role.as_deref(), &self.facts);
        self
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self.history.set_budget(limits.history_bytes);
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_thinking(mut self, thinking: bool) -> Self {
        self.thinking = thinking;
        self
    }

    /// The prompt this session sends, byte for byte, on every turn.
    pub fn system_prompt(&self) -> &str {
        &self.system
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Which provider is answering — `"anthropic"`, `"ollama"`.
    pub fn backend_name(&self) -> &str {
        self.backend.name()
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    pub fn history(&self) -> &History {
        &self.history
    }

    /// Forget the conversation and look at the machine again.
    ///
    /// The only way the environment description and the tool list are
    /// refreshed. Doing it mid-conversation would change what the model
    /// was told after it had already been told something else, and
    /// would throw away the provider's cache of the prefix.
    pub fn reset(&mut self) {
        self.history.clear();
        self.facts = self.tools.environment();
        self.declarations = self.tools.declarations();
        self.system = prompt::build(self.role.as_deref(), &self.facts);
    }

    /// Ask a question and see it through: tools, results, follow-up
    /// turns and all.
    ///
    /// Blocks until the model has finished, the user has stopped it, or
    /// something has failed. On failure the conversation is left in a
    /// state that can be asked again — the question stays, and no tool
    /// call is left without its result.
    pub fn ask(
        &mut self,
        question: impl Into<String>,
        approver: &mut dyn Approver,
        sink: &mut AgentSink<'_>,
    ) -> Result<Completion, AgentError> {
        self.ask_message(Message::user(question), approver, sink)
    }

    /// The same, for a turn that is more than text — a pasted file, an
    /// answer carrying tool results the caller produced itself.
    pub fn ask_message(
        &mut self,
        message: Message,
        approver: &mut dyn Approver,
        sink: &mut AgentSink<'_>,
    ) -> Result<Completion, AgentError> {
        self.history.push(message);
        self.run(approver, sink)
    }

    fn run(
        &mut self,
        approver: &mut dyn Approver,
        sink: &mut AgentSink<'_>,
    ) -> Result<Completion, AgentError> {
        let mut usage = Usage::default();
        let mut tools_run = 0u32;

        for turn in 1..=self.limits.max_turns {
            // Before the request rather than after the reply: what has
            // to fit is what is about to be sent.
            self.history.trim();
            let request = self.request();

            let mut answer = Answer::default();
            let sent = {
                let backend = &mut self.backend;
                backend.send(&request, &mut |event| answer.take(event, turn, sink))
            };
            usage = added(usage, answer.usage);

            // A failed turn is not written to the history. The
            // assistant said nothing that survived, the question is
            // still the last thing in the conversation, and asking
            // again therefore works.
            sent.map_err(AgentError::from)?;

            // A backend that ignored Flow::Stop and returned Ok anyway
            // is a bug, but the user still pressed stop.
            if answer.stopped {
                return Err(AgentError::Cancelled);
            }

            let Some(stop) = answer.stop.clone() else {
                // The contract is Start, then content, then exactly one
                // End. A backend that returned Ok without ending the
                // stream has left us unable to say whether the reply is
                // complete, and guessing is how a truncated answer gets
                // presented as a finished one.
                return Err(AgentError::Backend(BackendError::Protocol(
                    "the backend finished the turn without ending the stream".to_string(),
                )));
            };

            let blocks = std::mem::take(&mut answer.blocks);
            let reply = text_of(&blocks);
            if !blocks.is_empty() {
                self.history.push(Message::new(Role::Assistant, blocks));
            }

            // No tools asked for means the model is done — whatever it
            // gave as a stop reason. Some providers say `tool_use` and
            // then send no calls, and running the loop again on that
            // would ask the same question twice.
            if answer.calls.is_empty() {
                return Ok(Completion {
                    text: reply,
                    turns: turn,
                    tools_run,
                    usage,
                    stop,
                });
            }

            let (results, halted, ran) = self.run_tools(&answer.calls, approver, sink);
            tools_run += ran;

            // Every call gets its result, including the ones that never
            // ran, and they all travel in one message. Both providers
            // require exactly that, and it is the reason a cancelled
            // exchange can still be continued later.
            self.history.push(Message::new(Role::User, results));

            if let Some(err) = halted {
                return Err(err);
            }
        }

        Err(AgentError::TurnLimit {
            limit: self.limits.max_turns,
        })
    }

    /// Run one turn's worth of tool calls.
    ///
    /// Returns the results — one per call, always — whatever stopped it
    /// part way, and how many actually ran.
    fn run_tools(
        &mut self,
        calls: &[ToolCall],
        approver: &mut dyn Approver,
        sink: &mut AgentSink<'_>,
    ) -> (Vec<Content>, Option<AgentError>, u32) {
        let mut results = Vec::with_capacity(calls.len());
        let mut halted: Option<AgentError> = None;
        let mut ran = 0u32;

        for call in calls {
            if halted.is_some() {
                results.push(not_run(call));
                continue;
            }

            // Asked before anything happens, and by the registry rather
            // than by the loop reading the tool's name: only the tools
            // know which of them write.
            if let Effect::Change(change) = self.tools.effect(call) {
                match approver.approve(ApprovalRequest {
                    call,
                    change: &change,
                }) {
                    Decision::Allow => {}
                    Decision::Deny { reason } => {
                        let told = refusal(reason.as_deref());
                        if sink(AgentEvent::ToolDenied {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            reason: told.clone(),
                        }) == Flow::Stop
                        {
                            halted = Some(AgentError::Cancelled);
                        }
                        // A refusal is an answer, not an omission: the
                        // model is told, in words, that the user said
                        // no, so it changes plan instead of retrying.
                        results.push(Content::ToolResult {
                            id: call.id.clone(),
                            output: told,
                            is_error: true,
                        });
                        continue;
                    }
                    Decision::Cancel => {
                        halted = Some(AgentError::Cancelled);
                        results.push(not_run(call));
                        continue;
                    }
                }
            }

            if sink(AgentEvent::ToolStarted { call: call.clone() }) == Flow::Stop {
                halted = Some(AgentError::Cancelled);
                results.push(not_run(call));
                continue;
            }

            let output = self.tools.invoke(call);
            ran += 1;
            let is_error = output.is_error;
            results.push(Content::ToolResult {
                id: call.id.clone(),
                output: output.output,
                is_error,
            });

            if sink(AgentEvent::ToolFinished {
                id: call.id.clone(),
                name: call.name.clone(),
                is_error,
            }) == Flow::Stop
            {
                halted = Some(AgentError::Cancelled);
            }
        }

        (results, halted, ran)
    }

    /// The request for the next turn.
    ///
    /// The history is copied rather than borrowed because [`Request`]
    /// owns its messages, which is what lets a backend hold it across a
    /// blocking send. Copying a bounded conversation is nothing beside
    /// the round trip it is about to pay for.
    fn request(&self) -> Request {
        let mut request = Request::new(self.model.clone())
            .with_max_tokens(self.max_tokens)
            .with_thinking(self.thinking);

        if !self.system.is_empty() {
            request = request.with_system(self.system.clone());
        }

        request.messages = self.history.messages().to_vec();
        request.tools = self.declarations.clone();
        request
    }
}

/// One model turn as it arrives.
#[derive(Default)]
struct Answer {
    blocks: Vec<Content>,
    calls: Vec<ToolCall>,
    usage: Usage,
    stop: Option<StopReason>,
    stopped: bool,
}

impl Answer {
    /// Record one event and pass it on to the interface.
    fn take(&mut self, event: StreamEvent, turn: u32, sink: &mut AgentSink<'_>) -> Flow {
        let forward = match event {
            StreamEvent::Start { model } => Some(AgentEvent::TurnStarted { turn, model }),
            StreamEvent::Text(text) => {
                push_text(&mut self.blocks, &text);
                Some(AgentEvent::Text(text))
            }
            StreamEvent::Thinking(text) => {
                push_thinking(&mut self.blocks, &text);
                Some(AgentEvent::Thinking(text))
            }
            StreamEvent::ToolCall(call) => {
                // Kept in the assistant message in the order it arrived
                // — a provider that is sent the calls back in a
                // different order than it made them will not match them
                // to their results.
                self.blocks.push(Content::ToolUse(call.clone()));
                self.calls.push(call);
                // No event: the interface hears about a tool when it
                // runs, which is a moment later and the one that can
                // actually be waited on.
                None
            }
            StreamEvent::End { stop, usage } => {
                self.stop = Some(stop);
                self.usage = usage;
                None
            }
        };

        match forward {
            Some(event) => {
                let flow = sink(event);
                if flow == Flow::Stop {
                    self.stopped = true;
                }
                flow
            }
            None => Flow::Continue,
        }
    }
}

/// Append a text fragment to the assistant's turn.
///
/// Fragments are merged into one block rather than kept as hundreds of
/// tiny ones: the split points are an artefact of the network, they
/// mean nothing to a provider that is sent the turn back, and a message
/// of five hundred one-word blocks is five hundred envelopes of
/// overhead.
fn push_text(blocks: &mut Vec<Content>, fragment: &str) {
    if let Some(Content::Text(text)) = blocks.last_mut() {
        text.push_str(fragment);
        return;
    }
    blocks.push(Content::Text(fragment.to_string()));
}

/// The same for reasoning.
///
/// The signature is `None` because the event stream does not carry one
/// — a reader of [`StreamEvent::Thinking`] gets text and nothing else.
/// That is why replayed thinking is dropped rather than sent back on
/// Anthropic, which verifies the signature and refuses a block that was
/// rebuilt; keeping the text here is for the transcript, not the wire.
fn push_thinking(blocks: &mut Vec<Content>, fragment: &str) {
    if let Some(Content::Thinking { text, .. }) = blocks.last_mut() {
        text.push_str(fragment);
        return;
    }
    blocks.push(Content::Thinking {
        text: fragment.to_string(),
        signature: None,
    });
}

fn text_of(blocks: &[Content]) -> String {
    let mut out = String::new();
    for block in blocks {
        if let Content::Text(text) = block {
            out.push_str(text);
        }
    }
    out
}

/// What the model is told when the user says no.
///
/// The last sentence is there because the model would otherwise read a
/// failed tool as something to try again; being told that the refusal
/// came from a person is what makes it propose a different route.
fn refusal(reason: Option<&str>) -> String {
    let mut told = String::from("The user declined to run this tool.");
    if let Some(reason) = reason {
        let reason = reason.trim();
        if !reason.is_empty() {
            told.push_str(" They said: ");
            told.push_str(reason);
        }
    }
    told.push_str(" Do not call it again — say what you would do instead, or ask them what they would prefer.");
    told
}

fn not_run(call: &ToolCall) -> Content {
    Content::ToolResult {
        id: call.id.clone(),
        output: "This tool was not run: the user stopped the agent first.".to_string(),
        is_error: true,
    }
}

/// Two turns' usage, added.
///
/// Saturating rather than wrapping: a counter that has run out should
/// stick at "an enormous number of tokens", not start again at nothing.
fn added(a: Usage, b: Usage) -> Usage {
    Usage {
        input_tokens: a.input_tokens.saturating_add(b.input_tokens),
        output_tokens: a.output_tokens.saturating_add(b.output_tokens),
        cache_read_tokens: a.cache_read_tokens.saturating_add(b.cache_read_tokens),
        cache_write_tokens: a.cache_write_tokens.saturating_add(b.cache_write_tokens),
    }
}
