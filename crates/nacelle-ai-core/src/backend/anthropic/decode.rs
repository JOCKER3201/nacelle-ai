//! Anthropic's stream events, turned into the contract's [`StreamEvent`]s.
//!
//! The provider's stream is a series of edits to a message being built:
//! `message_start`, then content blocks that open, receive deltas and
//! close, then `message_delta` carrying the stop reason and the final
//! token counts, then `message_stop`. The contract's stream is flatter
//! and says less, and everything the two disagree about is settled here.
//!
//! Three of those disagreements are load-bearing.
//!
//! * **Tool arguments arrive in pieces.** `input_json_delta` fragments
//!   are halves of a JSON document, and half a JSON document is not a
//!   smaller one. They are buffered until `content_block_stop` and
//!   parsed once, so a receiver never sees arguments it cannot use. The
//!   fragments are never matched or edited as text.
//! * **A refusal is a failure, not a stop reason.** The provider reports
//!   it as HTTP 200 with `stop_reason: "refusal"`. Passing that on as a
//!   [`StopReason`] would let a caller treat a refused turn as a
//!   finished one, so it becomes [`BackendError::Refused`] and no
//!   [`StreamEvent::End`] is emitted.
//! * **The turn has to actually end.** A body that stops arriving
//!   mid-stream is an error, never a short answer.

use std::collections::HashMap;
use std::io::Read;

use serde_json::{Map, Value};

use crate::backend::{EventSink, Flow};
use crate::error::BackendError;
use crate::event::{StopReason, StreamEvent, Usage};
use crate::message::ToolCall;

use super::sse::{Frame, Frames};

/// How much buffered tool-argument JSON is too much. The same reasoning
/// as the framing ceiling: the body is not ours, so it gets a bound.
const MAX_ARGUMENT_BYTES: usize = 4 * 1024 * 1024;

/// Read one turn out of a response body.
///
/// `asked_for` is the model named in the request, used only if the
/// provider does not name one itself — [`StreamEvent::Start`] promises a
/// model, and an empty string would be a worse answer than a stale one.
pub(super) fn run(
    source: impl Read,
    asked_for: &str,
    sink: &mut EventSink<'_>,
) -> Result<(), BackendError> {
    let mut frames = Frames::new(source);
    let mut turn = Turn::default();

    while let Some(frame) = frames.next_frame()? {
        if turn.apply(&frame, asked_for, sink)? {
            return Ok(());
        }
    }

    // Falling out of the loop means the body ended without
    // `message_stop`: a dropped connection, a proxy timing out, a
    // provider incident. Whatever arrived is a fragment.
    Err(BackendError::Protocol(
        "the reply stopped before the model finished".to_string(),
    ))
}

/// What is known about the turn so far.
#[derive(Default)]
struct Turn {
    started: bool,
    /// Tool calls under construction, by content-block index. Only
    /// `tool_use` blocks need remembering; text and thinking are emitted
    /// as they arrive.
    calls: HashMap<u64, PartialCall>,
    usage: Usage,
    stop: Option<String>,
}

struct PartialCall {
    id: String,
    name: String,
    /// The `input_json_delta` fragments, concatenated and not yet parsed.
    arguments: String,
}

impl Turn {
    /// Apply one frame. Returns `true` once the turn has ended.
    fn apply(
        &mut self,
        frame: &Frame,
        asked_for: &str,
        sink: &mut EventSink<'_>,
    ) -> Result<bool, BackendError> {
        // `ping` carries nothing and exists only to hold the connection
        // open. Parsing it would be work for nothing.
        if frame.name == "ping" {
            return Ok(false);
        }

        let data = parse(&frame.data, &frame.name)?;

        match frame.name.as_str() {
            "message_start" => {
                let message = data.get("message");
                let model = field(message, "model")
                    .filter(|model| !model.is_empty())
                    .unwrap_or(asked_for)
                    .to_string();
                // The prompt is billed at message_start; the completion
                // is counted again at message_delta.
                self.absorb_usage(message.and_then(|message| message.get("usage")));
                self.started = true;
                emit(sink, StreamEvent::Start { model })?;
            }

            // Text and thinking blocks need nothing done when they open;
            // their content arrives as deltas. A `redacted_thinking`
            // block, which arrives whole and encrypted, is dropped here
            // for the same reason as a thinking signature: the contract's
            // stream has nowhere to carry it.
            "content_block_start" => {
                let index = index_of(&data, &frame.name)?;
                let block = data.get("content_block");
                if field(block, "type") == Some("tool_use") {
                    // Anthropic promises id and name here, before any
                    // argument fragment. Nothing else has to be
                    // remembered: the arguments are appended as they
                    // come.
                    self.calls.insert(
                        index,
                        PartialCall {
                            id: field(block, "id").unwrap_or_default().to_string(),
                            name: field(block, "name").unwrap_or_default().to_string(),
                            arguments: String::new(),
                        },
                    );
                }
            }

            "content_block_delta" => {
                let delta = data.get("delta");
                match field(delta, "type") {
                    Some("text_delta") => {
                        let text = field(delta, "text").unwrap_or_default();
                        emit(sink, StreamEvent::Text(text.to_string()))?;
                    }
                    Some("thinking_delta") => {
                        let thinking = field(delta, "thinking").unwrap_or_default();
                        emit(sink, StreamEvent::Thinking(thinking.to_string()))?;
                    }
                    Some("input_json_delta") => {
                        let index = index_of(&data, &frame.name)?;
                        let fragment = field(delta, "partial_json").unwrap_or_default();
                        // An index we are not tracking belongs to a block
                        // this backend did not open — a server-side tool,
                        // say. Dropping the fragment is right: there is
                        // no call for it to complete.
                        if let Some(call) = self.calls.get_mut(&index) {
                            if call.arguments.len() + fragment.len() > MAX_ARGUMENT_BYTES {
                                return Err(BackendError::Protocol(format!(
                                    "the arguments for {} never stopped arriving",
                                    call.name
                                )));
                            }
                            call.arguments.push_str(fragment);
                        }
                    }
                    // `signature_delta` signs a thinking block so it can
                    // be replayed as history. StreamEvent has nowhere to
                    // put it, so it is dropped here rather than half-kept
                    // — see the note on replaying thinking in `body`.
                    _ => {}
                }
            }

            "content_block_stop" => {
                let index = index_of(&data, &frame.name)?;
                if let Some(call) = self.calls.remove(&index) {
                    emit(sink, StreamEvent::ToolCall(finish(call)?))?;
                }
            }

            "message_delta" => {
                let delta = data.get("delta");
                self.absorb_usage(data.get("usage"));

                if let Some(stop) = field(delta, "stop_reason") {
                    // Read before anything else is done with the turn.
                    // A refusal often carries no content at all and
                    // sometimes carries half of one, and `stop_details`
                    // is null as often as not — so it is asked for by
                    // name and never indexed into blind.
                    if stop == REFUSAL {
                        let details = delta
                            .and_then(|delta| delta.get("stop_details"))
                            .or_else(|| data.get("stop_details"));
                        return Err(BackendError::Refused {
                            category: field(details, "category").map(str::to_string),
                            explanation: field(details, "explanation").map(str::to_string),
                        });
                    }
                    self.stop = Some(stop.to_string());
                }
            }

            "message_stop" => {
                if !self.started {
                    return Err(BackendError::Protocol(
                        "the reply ended before it began".to_string(),
                    ));
                }
                emit(
                    sink,
                    StreamEvent::End {
                        stop: stop_reason(self.stop.as_deref()),
                        usage: self.usage,
                    },
                )?;
                return Ok(true);
            }

            "error" => return Err(error_frame(&data)),

            // An event added after this was written is not a failure.
            _ => {}
        }

        Ok(false)
    }

    /// Take whichever token counts this frame reported. Providers report
    /// different subsets at different points, and a field that is absent
    /// means "no news", not zero.
    fn absorb_usage(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage else { return };
        if let Some(tokens) = count(usage, "input_tokens") {
            self.usage.input_tokens = tokens;
        }
        if let Some(tokens) = count(usage, "output_tokens") {
            self.usage.output_tokens = tokens;
        }
        if let Some(tokens) = count(usage, "cache_read_input_tokens") {
            self.usage.cache_read_tokens = tokens;
        }
        if let Some(tokens) = count(usage, "cache_creation_input_tokens") {
            self.usage.cache_write_tokens = tokens;
        }
    }
}

/// The provider's word for a request its safety classifiers declined.
const REFUSAL: &str = "refusal";

/// A tool call, once its arguments are whole.
fn finish(call: PartialCall) -> Result<ToolCall, BackendError> {
    // A tool with no arguments gets no `input_json_delta` at all, so an
    // empty buffer means an empty object rather than a truncated one.
    let input = if call.arguments.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        serde_json::from_str(&call.arguments).map_err(|err| {
            BackendError::Protocol(format!(
                "the arguments for {} were not JSON: {err}",
                call.name
            ))
        })?
    };

    Ok(ToolCall {
        id: call.id,
        name: call.name,
        input,
    })
}

/// The provider's stop reason, in the contract's terms.
///
/// `pause_turn` has no counterpart because it is not an ending: the model
/// ran out of room mid-task and the conversation is resent unchanged so
/// it can carry on. It is kept verbatim under
/// [`StopReason::Other`] so a caller can recognise it — see
/// [`STOP_PAUSE_TURN`](super::STOP_PAUSE_TURN).
fn stop_reason(raw: Option<&str>) -> StopReason {
    match raw {
        Some("end_turn") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("stop_sequence") => StopReason::StopSequence,
        Some(other) => StopReason::Other(other.to_string()),
        // `message_stop` without a preceding `message_delta`. Not worth
        // failing over, but not worth inventing `end_turn` for either.
        None => StopReason::Other("unspecified".to_string()),
    }
}

/// An `error` frame: the provider giving up part-way through a reply.
///
/// The turn is over either way; the only question is whether asking again
/// could work, so that is what the mapping is about.
fn error_frame(data: &Value) -> BackendError {
    let error = data.get("error");
    let kind = field(error, "type").unwrap_or("error");
    let message = field(error, "message")
        .unwrap_or("no detail given")
        .to_string();

    match kind {
        // The provider is saturated. This is the common one, and it is
        // worth retrying.
        "overloaded_error" => BackendError::Server {
            status: 529,
            message,
        },
        "api_error" => BackendError::Server {
            status: 500,
            message,
        },
        "rate_limit_error" => BackendError::RateLimited {
            // Mid-stream there are no headers left to carry one.
            retry_after: None,
            message,
        },
        "authentication_error" | "permission_error" => BackendError::Auth(message),
        // Something this version has no rule for. Reported rather than
        // guessed at, which at least means it is never silently retried.
        _ => BackendError::Protocol(format!("{kind}: {message}")),
    }
}

fn parse(data: &str, name: &str) -> Result<Value, BackendError> {
    serde_json::from_str(data)
        .map_err(|err| BackendError::Protocol(format!("the {name} event was not JSON: {err}")))
}

fn index_of(data: &Value, name: &str) -> Result<u64, BackendError> {
    data.get("index")
        .and_then(Value::as_u64)
        .ok_or_else(|| BackendError::Protocol(format!("the {name} event named no content block")))
}

/// A string field of an optional object, without a chain of `match` at
/// every call site.
fn field<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a str> {
    value?.get(key)?.as_str()
}

fn count(usage: &Value, key: &str) -> Option<u32> {
    usage
        .get(key)?
        .as_u64()
        .map(|tokens| u32::try_from(tokens).unwrap_or(u32::MAX))
}

fn emit(sink: &mut EventSink<'_>, event: StreamEvent) -> Result<(), BackendError> {
    // The one way a turn is cancelled: the receiver says stop, the
    // backend drops the connection, and no End is ever emitted.
    if sink(event) == Flow::Stop {
        return Err(BackendError::Cancelled);
    }
    Ok(())
}
