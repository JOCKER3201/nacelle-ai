//! The request body: the conversation as the Messages API wants it.
//!
//! Most of this is transliteration. The parts that are not are the parts
//! where the obvious encoding is a 400:
//!
//! * **`thinking` has to be asked for.** On Opus 4.8 leaving the field
//!   out means no thinking at all, so wanting it means saying
//!   `{"type": "adaptive"}` in as many words. Depth is
//!   `output_config.effort`; `budget_tokens` was removed from these
//!   models and sending it is rejected.
//! * **No sampling parameters.** `temperature`, `top_p` and `top_k` were
//!   removed too, and any of them is a 400. Behaviour is steered by the
//!   prompt instead — which is why nothing here has a knob for it.
//! * **The system prompt is where caching pays.** It is the one part of
//!   a request that is byte-identical from turn to turn, so it carries
//!   the cache breakpoint.
//!
//! And the one that is not about the wire format at all: [`build`] takes
//! a [`Sealed`] request, not a [`Request`]. This is the last function
//! before the bytes exist, so it is where "everything that leaves has
//! been through the layers" stops being a rule somebody has to remember
//! and becomes something the compiler will not let anyone forget — see
//! [`supervise::seal`](crate::supervise::seal).

use serde_json::{json, Map, Value};

use crate::message::{Content, Message, Request, Role};
use crate::supervise::seal::Sealed;

use super::Effort;

/// Build the body for one turn.
pub(super) fn build(sealed: &Sealed, effort: Effort, summarise_thinking: bool) -> Value {
    let request = sealed.request();
    let (system, messages) = conversation(request);

    let mut body = Map::new();
    body.insert("model".to_string(), json!(request.model));
    body.insert("max_tokens".to_string(), json!(request.max_tokens));
    // Always streamed. A long reply on a non-streamed request sits on an
    // idle connection until something in the middle gives up on it, and
    // the agent wants the text as it arrives regardless.
    body.insert("stream".to_string(), json!(true));
    body.insert("messages".to_string(), json!(messages));
    // Sent even at the default, so the request says what it wants
    // instead of relying on the provider's default staying put.
    body.insert(
        "output_config".to_string(),
        json!({ "effort": effort.as_str() }),
    );

    if !system.is_empty() {
        // `cache_control` on the last system block caches the tools and
        // the system prompt together — they are rendered in that order,
        // and the breakpoint covers everything before it.
        //
        // Worth knowing before wondering why a bill did not move: on
        // Opus 4.8 the shortest prefix that can be cached at all is 4096
        // tokens. A shorter one is simply not cached — no error, no
        // warning, full price. The environment description this agent
        // sends is long and fixed, which is exactly the case this pays
        // for.
        body.insert(
            "system".to_string(),
            json!([{
                "type": "text",
                "text": system,
                "cache_control": { "type": "ephemeral" },
            }]),
        );
    }

    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.input_schema,
                })
            })
            .collect();
        body.insert("tools".to_string(), json!(tools));
    }

    if request.thinking {
        body.insert(
            "thinking".to_string(),
            if summarise_thinking {
                // Without this the thinking blocks arrive with empty
                // text: the model still thinks and is still billed for
                // it, but a reader sees a long silence and then an
                // answer. The agent shows its progress, so it asks for
                // the summary.
                json!({ "type": "adaptive", "display": "summarized" })
            } else {
                json!({ "type": "adaptive" })
            },
        );
    }

    Value::Object(body)
}

/// Split the request into the system prompt and the message list.
///
/// A leading run of [`Role::System`] messages is folded into the system
/// prompt. That is not a convenience: the endpoint refuses a system
/// message in first position, so a leading one can only ever have meant
/// the system prompt. Later ones are sent as system-role messages, which
/// is how an instruction is delivered mid-conversation without editing
/// the cached prefix — that needs Opus 4.8.
fn conversation(request: &Request) -> (String, Vec<Value>) {
    let mut system: Vec<String> = Vec::new();
    if let Some(prompt) = &request.system {
        if !prompt.is_empty() {
            system.push(prompt.clone());
        }
    }

    let mut messages = Vec::new();
    let mut leading = true;

    for message in &request.messages {
        if leading && message.role == Role::System {
            let text = message.text();
            if !text.is_empty() {
                system.push(text);
            }
            continue;
        }
        leading = false;

        let content = blocks(message);
        // A message with no content at all is a 400, and there is
        // nothing to say on behalf of a turn that has nothing in it.
        if content.is_empty() {
            continue;
        }

        messages.push(json!({ "role": role(message.role), "content": content }));
    }

    (system.join("\n\n"), messages)
}

fn role(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// One message's content blocks.
///
/// Tool results are ordinary blocks here, which is what makes the rule
/// they have to obey easy to keep: every result for a turn belongs in one
/// user message. Splitting them across several is rejected, and a missing
/// one is rejected too — one `tool_result` per `tool_use`, always,
/// failures included and marked with `is_error`.
fn blocks(message: &Message) -> Vec<Value> {
    let mut blocks = Vec::new();

    for content in &message.content {
        match content {
            Content::Text(text) => blocks.push(json!({ "type": "text", "text": text })),

            // The signature is what makes a thinking block replayable:
            // the endpoint verifies it and rejects a block that was
            // edited or rebuilt. An unsigned one therefore cannot be sent
            // at all, and is dropped — that costs the model a little
            // context, where sending it would cost the whole turn.
            Content::Thinking { text, signature } => {
                if let Some(signature) = signature {
                    blocks.push(json!({
                        "type": "thinking",
                        "thinking": text,
                        "signature": signature,
                    }));
                }
            }

            Content::ToolUse(call) => blocks.push(json!({
                "type": "tool_use",
                "id": call.id,
                "name": call.name,
                "input": call.input,
            })),

            Content::ToolResult {
                id,
                output,
                is_error,
            } => {
                let mut block = json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": output,
                });
                // Only when it is true: the field means "this tool
                // failed", and saying so about every result that
                // succeeded is noise in the model's context.
                if *is_error {
                    block["is_error"] = json!(true);
                }
                blocks.push(block);
            }
        }
    }

    blocks
}
