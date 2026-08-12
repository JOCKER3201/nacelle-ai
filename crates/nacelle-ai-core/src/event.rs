//! What a reply looks like while it is still arriving: one enum every
//! backend emits, whoever answered.
//!
//! This is the point of the whole backend contract. Providers disagree
//! about almost everything on the wire — Anthropic sends SSE frames with
//! `content_block_delta`, Ollama sends newline-delimited JSON objects,
//! and the two describe a tool call completely differently — yet a
//! reader of [`StreamEvent`] cannot tell them apart. Anything a provider
//! does that is not visible here is, by construction, the backend's
//! problem.
//!
//! The rule that makes tool calls line up: **a tool call is only
//! announced once it is whole.** Ollama already hands over the entire
//! call, so its backend forwards it. Anthropic sends the arguments as a
//! run of `input_json_delta` fragments, so its backend buffers them
//! until `content_block_stop` and parses once. Both then emit a single
//! [`StreamEvent::ToolCall`], and the receiver sees the same stream.
//!
//! A well-formed stream is:
//!
//! ```text
//! Start
//!   (Text | Thinking | ToolCall)*
//! End
//! ```
//!
//! `End` is emitted exactly once, and only when the turn really ended. A
//! turn that failed part-way through produces no `End` — the backend
//! returns [`BackendError`](crate::error::BackendError) instead, so a
//! caller can never mistake a broken stream for a finished one.

use crate::message::ToolCall;

/// One increment of a reply.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    /// The turn has begun. Carries the model the provider says actually
    /// answered, which is not always the one that was asked for.
    Start { model: String },
    /// A fragment of the visible answer. Fragments are not lines, words
    /// or characters — concatenating them all yields the answer, and
    /// nothing else about their boundaries is promised.
    Text(String),
    /// A fragment of the model's reasoning, under the same rules as
    /// [`StreamEvent::Text`]. Only appears when the request asked for
    /// thinking and the provider supports it.
    Thinking(String),
    /// The model wants a tool run. Complete: arguments are parsed and
    /// final.
    ToolCall(ToolCall),
    /// The turn is over. The last event of any successful stream.
    End { stop: StopReason, usage: Usage },
}

/// Why the model stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// The model finished what it had to say.
    EndTurn,
    /// The model is waiting for tool results. The caller runs the tools
    /// it was sent, appends the results, and asks again.
    ToolUse,
    /// The reply hit `max_tokens` and is cut off mid-thought. Whatever
    /// arrived is a fragment, not an answer.
    MaxTokens,
    /// A configured stop sequence was produced.
    StopSequence,
    /// Something the provider reported that does not map onto the above.
    /// Kept as text rather than dropped, so a new provider behaviour
    /// shows up in a log instead of vanishing.
    Other(String),
}

/// What the turn cost.
///
/// Zero means "not reported". Providers vary in which of these they
/// return, and a provider that reports nothing is not an error.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Prompt tokens served from the provider's cache — billed far below
    /// `input_tokens`, so they are counted apart rather than folded in.
    pub cache_read_tokens: u32,
    /// Prompt tokens written into the provider's cache this turn.
    pub cache_write_tokens: u32,
}

impl Usage {
    /// Everything the provider charged for, cached or not.
    pub fn total_tokens(&self) -> u64 {
        u64::from(self.input_tokens)
            + u64::from(self.output_tokens)
            + u64::from(self.cache_read_tokens)
            + u64::from(self.cache_write_tokens)
    }
}
