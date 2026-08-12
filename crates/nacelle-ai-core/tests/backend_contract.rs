//! The backend contract, exercised against two stubs shaped like the two
//! providers that will implement it for real.
//!
//! The claim under test is the one the whole design rests on: a provider
//! that hands over a tool call in one piece (Ollama) and a provider that
//! dribbles the arguments out as JSON fragments (Anthropic's
//! `input_json_delta`) must produce the *same* event stream. If these
//! two stubs ever disagree, the agent loop can tell who answered — and
//! then the trait has stopped earning its keep.

use nacelle_ai::{
    Backend, BackendError, EventSink, Flow, Request, StopReason, StreamEvent, ToolCall, Usage,
};
use serde_json::json;

const MODEL: &str = "test-model";

fn usage() -> Usage {
    Usage {
        input_tokens: 11,
        output_tokens: 7,
        ..Usage::default()
    }
}

/// What both stubs are asked to produce.
fn expected() -> Vec<StreamEvent> {
    vec![
        StreamEvent::Start {
            model: MODEL.to_string(),
        },
        StreamEvent::Text("Checking the weather.".to_string()),
        StreamEvent::ToolCall(ToolCall {
            id: "call-1".to_string(),
            name: "get_weather".to_string(),
            input: json!({ "location": "Earth", "unit": "celsius" }),
        }),
        StreamEvent::End {
            stop: StopReason::ToolUse,
            usage: usage(),
        },
    ]
}

/// A provider that delivers a whole tool call at once, the way Ollama
/// does. Its backend has nothing to reassemble.
struct WholeCallBackend;

impl Backend for WholeCallBackend {
    fn name(&self) -> &str {
        "whole-call"
    }

    fn send(&mut self, _request: &Request, sink: &mut EventSink<'_>) -> Result<(), BackendError> {
        for event in expected() {
            if sink(event) == Flow::Stop {
                return Err(BackendError::Cancelled);
            }
        }
        Ok(())
    }
}

/// A provider that streams the arguments as JSON fragments, the way
/// Anthropic does. The buffering here is exactly what the real backend
/// has to do between `content_block_start` and `content_block_stop`.
struct ChunkedCallBackend;

impl Backend for ChunkedCallBackend {
    fn name(&self) -> &str {
        "chunked-call"
    }

    fn send(&mut self, _request: &Request, sink: &mut EventSink<'_>) -> Result<(), BackendError> {
        if sink(StreamEvent::Start {
            model: MODEL.to_string(),
        }) == Flow::Stop
        {
            return Err(BackendError::Cancelled);
        }

        // Text arrives in fragments too; those go straight out, because
        // text has no structure that a half of it could violate.
        for fragment in ["Checking ", "the weather."] {
            if sink(StreamEvent::Text(fragment.to_string())) == Flow::Stop {
                return Err(BackendError::Cancelled);
            }
        }

        // Arguments are different: half of a JSON document is not a
        // smaller JSON document. They are buffered until the provider
        // says the block is closed, then parsed once.
        let mut buffer = String::new();
        for fragment in [r#"{"locati"#, r#"on": "Earth", "un"#, r#"it": "celsius"}"#] {
            buffer.push_str(fragment);
        }

        let input = serde_json::from_str(&buffer)
            .map_err(|err| BackendError::Protocol(format!("tool arguments: {err}")))?;

        if sink(StreamEvent::ToolCall(ToolCall {
            id: "call-1".to_string(),
            name: "get_weather".to_string(),
            input,
        })) == Flow::Stop
        {
            return Err(BackendError::Cancelled);
        }

        if sink(StreamEvent::End {
            stop: StopReason::ToolUse,
            usage: usage(),
        }) == Flow::Stop
        {
            return Err(BackendError::Cancelled);
        }

        Ok(())
    }
}

fn collect(backend: &mut dyn Backend) -> Result<Vec<StreamEvent>, BackendError> {
    let mut events = Vec::new();
    let request = Request::new(MODEL);
    backend.send(&request, &mut |event| {
        events.push(event);
        Flow::Continue
    })?;
    Ok(events)
}

/// Text fragments may be split differently; the assembled answer may not.
fn text_of(events: &[StreamEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Text(fragment) => Some(fragment.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_calls_of(events: &[StreamEvent]) -> Vec<&ToolCall> {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect()
}

#[test]
fn both_providers_produce_the_same_tool_call() {
    let whole = collect(&mut WholeCallBackend).expect("whole-call stream");
    let chunked = collect(&mut ChunkedCallBackend).expect("chunked stream");

    assert_eq!(tool_calls_of(&whole), tool_calls_of(&chunked));
    assert_eq!(text_of(&whole), text_of(&chunked));
    assert_eq!(whole.first(), chunked.first(), "same Start");
    assert_eq!(whole.last(), chunked.last(), "same End");
}

#[test]
fn a_tool_call_is_only_announced_once_it_is_whole() {
    // Not one event per fragment: a receiver must never see arguments it
    // cannot parse, so partial JSON has no representation in the stream.
    let events = collect(&mut ChunkedCallBackend).expect("stream");
    let calls = tool_calls_of(&events);

    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].input["location"], json!("Earth"));
    assert_eq!(calls[0].input["unit"], json!("celsius"));
}

#[test]
fn a_stream_starts_once_and_ends_once() {
    for backend in [
        &mut WholeCallBackend as &mut dyn Backend,
        &mut ChunkedCallBackend,
    ] {
        let events = collect(backend).expect("stream");

        assert!(matches!(events.first(), Some(StreamEvent::Start { .. })));
        assert!(matches!(events.last(), Some(StreamEvent::End { .. })));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::End { .. }))
                .count(),
            1
        );
    }
}

#[test]
fn stopping_the_sink_cancels_the_turn_without_ending_it() {
    // The distinction matters: End means the model finished, and a
    // cancelled turn must not be able to masquerade as one.
    let mut events = Vec::new();
    let request = Request::new(MODEL);
    let result = WholeCallBackend.send(&request, &mut |event| {
        let first_text = matches!(event, StreamEvent::Text(_));
        events.push(event);
        if first_text {
            Flow::Stop
        } else {
            Flow::Continue
        }
    });

    assert_eq!(result, Err(BackendError::Cancelled));
    assert!(!events
        .iter()
        .any(|e| matches!(e, StreamEvent::End { .. })));
    assert!(!BackendError::Cancelled.is_retryable());
}

#[test]
fn a_backend_can_be_chosen_at_runtime() {
    // Object safety is a requirement, not an accident: which provider
    // answers is a configuration value, so the agent loop holds a boxed
    // trait object.
    let mut backend: Box<dyn Backend> = Box::new(ChunkedCallBackend);
    assert_eq!(backend.name(), "chunked-call");
    // Start, two text fragments, the assembled call, End.
    assert_eq!(collect(backend.as_mut()).expect("stream").len(), 5);
}

#[test]
fn errors_say_whether_trying_again_is_worth_it() {
    use std::time::Duration;

    assert!(BackendError::Network("reset".into()).is_retryable());
    assert!(BackendError::Server {
        status: 529,
        message: "overloaded".into()
    }
    .is_retryable());
    assert!(!BackendError::Auth("401".into()).is_retryable());
    assert!(!BackendError::Refused {
        category: Some("cyber".into()),
        explanation: None
    }
    .is_retryable());
    assert!(!BackendError::Protocol("bad frame".into()).is_retryable());

    let limited = BackendError::RateLimited {
        retry_after: Some(Duration::from_secs(30)),
        message: "slow down".into(),
    };
    assert!(limited.is_retryable());
    assert_eq!(limited.retry_after(), Some(Duration::from_secs(30)));
    assert!(limited.to_string().contains("30s"));
}
