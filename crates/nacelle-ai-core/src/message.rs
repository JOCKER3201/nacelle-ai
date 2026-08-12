//! The conversation as the agent sees it, and the tools it may offer the
//! model: roles, content blocks, tool declarations and the [`Request`] a
//! backend is asked to run.
//!
//! These types are deliberately not any provider's wire format. A
//! backend translates them on the way out and translates the reply back
//! into [`StreamEvent`](crate::event::StreamEvent)s on the way in, which
//! is the whole reason the agent loop can stay ignorant of who answered.

use serde_json::Value;

/// Who a message came from.
///
/// `System` is a role here even though some providers carry the system
/// prompt in a separate field rather than in the message list: that is a
/// wire-format detail for the backend to sort out, not a distinction the
/// conversation model should have to make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
}

/// One tool invocation, complete.
///
/// A call only exists once its arguments are whole. Providers differ
/// wildly on how the arguments arrive — Ollama hands over the entire
/// call in one piece, Anthropic dribbles the JSON out as
/// `input_json_delta` fragments — and reconciling that is the backend's
/// job, not the caller's. See [`StreamEvent`](crate::event::StreamEvent).
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCall {
    /// The provider's identifier for this call. It has to be echoed back
    /// on the matching [`Content::ToolResult`], so it is carried
    /// verbatim and never interpreted.
    pub id: String,
    pub name: String,
    /// Arguments, already parsed. A backend that could not parse them
    /// reports [`BackendError::Protocol`](crate::error::BackendError::Protocol)
    /// rather than passing along half a document.
    pub input: Value,
}

/// A piece of one message.
///
/// A single turn is a list of these because a model may narrate, think
/// and call a tool in one reply, and the order it did so in matters when
/// the turn is sent back as history.
#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    Text(String),
    /// Reasoning the model chose to show.
    ///
    /// `signature` exists because some providers sign thinking blocks and
    /// reject a conversation whose blocks were edited or rebuilt. Keep it
    /// with the text and hand both back unchanged.
    Thinking {
        text: String,
        signature: Option<String>,
    },
    ToolUse(ToolCall),
    /// What the tool answered. `id` matches [`ToolCall::id`].
    ToolResult {
        id: String,
        output: String,
        is_error: bool,
    },
}

/// One turn of the conversation.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
}

impl Message {
    pub fn new(role: Role, content: Vec<Content>) -> Self {
        Message { role, content }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Message::new(Role::User, vec![Content::Text(text.into())])
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Message::new(Role::Assistant, vec![Content::Text(text.into())])
    }

    pub fn system(text: impl Into<String>) -> Self {
        Message::new(Role::System, vec![Content::Text(text.into())])
    }

    /// The plain text of this turn, with everything else dropped. Useful
    /// for logs and for callers that only ever wanted the prose.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            if let Content::Text(t) = block {
                out.push_str(t);
            }
        }
        out
    }
}

/// A tool the model is told it may call.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolDeclaration {
    pub name: String,
    /// The model picks tools by reading this, so it says *when* to call
    /// the tool, not just what it does.
    pub description: String,
    /// JSON Schema for the arguments. Kept as a [`Value`] because every
    /// provider wants this same schema in a slightly different envelope
    /// and none of them want it typed.
    pub input_schema: Value,
}

impl ToolDeclaration {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        ToolDeclaration {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }
}

/// Everything a backend needs for one turn.
///
/// There is no conversation object holding state between turns: the
/// caller owns the history and resends it. Providers are stateless the
/// same way, and a stateful wrapper here would only be a second place
/// for the history to be wrong.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// The provider's model identifier, passed through untouched. The
    /// core does not keep a list of model names; going stale is worse
    /// than being uninformed.
    pub model: String,
    /// The system prompt. Separate from `messages` because most
    /// providers treat it separately, and because it is the part a
    /// caller most often keeps byte-identical between turns so the
    /// provider can cache it.
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDeclaration>,
    pub max_tokens: u32,
    /// Ask the model to show its reasoning where the provider supports
    /// it. A backend whose provider has no such notion ignores this and
    /// simply emits no [`StreamEvent::Thinking`](crate::event::StreamEvent::Thinking).
    pub thinking: bool,
}

impl Request {
    pub fn new(model: impl Into<String>) -> Self {
        Request {
            model: model.into(),
            system: None,
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: DEFAULT_MAX_TOKENS,
            thinking: false,
        }
    }

    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    pub fn with_message(mut self, message: Message) -> Self {
        self.messages.push(message);
        self
    }

    pub fn with_tool(mut self, tool: ToolDeclaration) -> Self {
        self.tools.push(tool);
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
}

/// Enough room for a real answer, low enough that a runaway reply stops
/// on its own. Callers that need more say so.
pub const DEFAULT_MAX_TOKENS: u32 = 8192;
