//! The conversation so far, and how it is cut down when it outgrows its
//! budget.
//!
//! A history that only grows ends the same way every time: one turn is
//! refused for exceeding the context window, and every turn after it is
//! refused too, because the thing that made the request too big is
//! still in it. So there is a budget, and when the conversation passes
//! it the oldest of it goes.
//!
//! What may be dropped is the whole point. Two rules, and both are
//! about requests the provider would otherwise reject:
//!
//! * **A tool call and its result fall together.** A `tool_use` with no
//!   `tool_result` after it, or a `tool_result` with no `tool_use`
//!   before it, is a 400 on Anthropic and nonsense on Ollama. Trimming
//!   therefore works in *exchanges* — from one real user message to
//!   just before the next — never in individual messages. A pair cannot
//!   straddle that boundary, so it cannot be broken by dropping one.
//! * **The conversation still begins with the user.** The endpoints
//!   want the first message to be a user message; cutting to an
//!   exchange boundary means it always is.
//!
//! The newest exchange is never dropped, whatever it costs. It contains
//! the question being answered right now, and a request that is too
//! large is a better failure than a request that no longer contains
//! what was asked.
//!
//! The budget is in bytes of text rather than tokens because there is
//! no tokeniser here and adding one would mean adding a model file and
//! a dependency to the desktop. Bytes are a monotone proxy — roughly
//! four to the token for prose, fewer for JSON — and the job is to
//! bound growth, not to bill for it.

use std::collections::HashMap;

use crate::message::{Content, Message, Role};

/// The conversation, with a ceiling on how much of it is kept.
#[derive(Clone, Debug)]
pub struct History {
    messages: Vec<Message>,
    budget: usize,
}

impl History {
    /// A new, empty history that will be trimmed towards `budget` bytes.
    pub fn new(budget: usize) -> Self {
        History {
            messages: Vec::new(),
            budget,
        }
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    pub fn budget(&self) -> usize {
        self.budget
    }

    /// Change the ceiling. Takes effect at the next [`History::trim`]
    /// rather than immediately: the agent trims before it sends, and
    /// dropping messages the moment a setting changed would do it at a
    /// point nobody is watching.
    pub fn set_budget(&mut self, budget: usize) {
        self.budget = budget;
    }

    pub fn clear(&mut self) {
        self.messages.clear();
    }

    /// Roughly what the conversation weighs, in bytes of text.
    pub fn size(&self) -> usize {
        self.messages.iter().map(message_size).sum()
    }

    /// Add a turn.
    ///
    /// A message of the same role as the last one is merged into it
    /// rather than appended. Both providers expect the roles to
    /// alternate, and two user messages in a row is exactly what
    /// happens when the user types again while the previous exchange
    /// ended on tool results — which is a real sequence, not a misuse,
    /// so it is fixed here instead of being pushed onto every caller.
    pub fn push(&mut self, message: Message) {
        if message.content.is_empty() {
            // Nothing to say on behalf of a turn with nothing in it,
            // and an empty message is a 400 on Anthropic.
            return;
        }

        match self.messages.last_mut() {
            Some(last) if last.role == message.role => {
                last.content.extend(message.content);
            }
            _ => self.messages.push(message),
        }
    }

    /// Drop the oldest exchanges until the history fits its budget.
    ///
    /// Returns how many messages went. Stops early rather than break
    /// either invariant: the newest exchange stays whole even when it
    /// alone is over budget.
    pub fn trim(&mut self) -> usize {
        // Sizes are computed once and kept in step with the drains,
        // because measuring a tool result means serialising its
        // arguments and doing that repeatedly for every candidate cut
        // is work for nothing.
        let mut sizes: Vec<usize> = self.messages.iter().map(message_size).collect();
        let mut total: usize = sizes.iter().sum();
        let head = self.head_len();
        let mut dropped = 0;

        while total > self.budget {
            let starts = self.exchange_starts();
            // One exchange left (or none): there is nothing that can go
            // without taking the current question with it.
            if starts.len() < 2 {
                break;
            }

            let cut = starts[1];
            total -= sizes[head..cut].iter().sum::<usize>();
            sizes.drain(head..cut);
            self.messages.drain(head..cut);
            dropped += cut - head;
        }

        dropped
    }

    /// Whether every tool call in the history has its result and every
    /// result has its call, in that order.
    ///
    /// This is the invariant trimming must not break, stated as
    /// something that can be checked rather than only described. A
    /// history that fails it will be refused by the provider, so it is
    /// worth asserting after anything that edits the conversation.
    pub fn pairs_are_complete(&self) -> bool {
        let mut calls: HashMap<&str, usize> = HashMap::new();
        let mut results: HashMap<&str, usize> = HashMap::new();

        for (index, message) in self.messages.iter().enumerate() {
            for block in &message.content {
                match block {
                    Content::ToolUse(call) => {
                        calls.insert(call.id.as_str(), index);
                    }
                    Content::ToolResult { id, .. } => {
                        results.insert(id.as_str(), index);
                    }
                    _ => {}
                }
            }
        }

        if calls.len() != results.len() {
            return false;
        }

        calls.iter().all(|(id, called_at)| {
            results
                .get(id)
                .is_some_and(|answered_at| answered_at >= called_at)
        })
    }

    /// Leading system messages, which are never dropped.
    ///
    /// The agent keeps its system prompt out of the message list
    /// entirely — it belongs in [`Request::system`](crate::message::Request::system)
    /// — but a caller may have put standing instructions at the front,
    /// and dropping those would silently change what the agent is
    /// while it is answering.
    fn head_len(&self) -> usize {
        self.messages
            .iter()
            .take_while(|message| message.role == Role::System)
            .count()
    }

    /// Where each exchange begins: a user message that is the user
    /// speaking, rather than one carrying tool results.
    ///
    /// The distinction is what keeps pairs together. Tool results
    /// travel in a user message — that is how both providers want them
    /// — but such a message continues the exchange the model started,
    /// so it can never be a place to cut.
    fn exchange_starts(&self) -> Vec<usize> {
        let head = self.head_len();
        self.messages
            .iter()
            .enumerate()
            .skip(head)
            .filter(|(_, message)| {
                message.role == Role::User
                    && !message
                        .content
                        .iter()
                        .any(|block| matches!(block, Content::ToolResult { .. }))
            })
            .map(|(index, _)| index)
            .collect()
    }
}

/// What one message weighs, near enough.
///
/// The constants are the envelope every provider wraps a message and a
/// block in — role names, block types, brackets and quotes. They are
/// not accurate for any particular provider and are not meant to be:
/// leaving them out would make a conversation of many tiny messages
/// look free, which is the case where the estimate matters most.
fn message_size(message: &Message) -> usize {
    const PER_MESSAGE: usize = 32;
    const PER_BLOCK: usize = 32;

    let mut size = PER_MESSAGE;
    for block in &message.content {
        size += PER_BLOCK
            + match block {
                Content::Text(text) => text.len(),
                Content::Thinking { text, signature } => {
                    text.len() + signature.as_ref().map_or(0, String::len)
                }
                Content::ToolUse(call) => {
                    call.id.len() + call.name.len() + call.input.to_string().len()
                }
                Content::ToolResult { id, output, .. } => id.len() + output.len(),
            };
    }
    size
}
