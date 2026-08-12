//! The Anthropic backend, exercised against a transport that is a
//! `Vec<u8>` rather than a socket.
//!
//! Nothing here touches the network, and that is the point rather than a
//! limitation. The cases worth testing are a frame split at an awkward
//! byte, a refusal that arrives after half an answer, a 529 in the middle
//! of a stream and a rate limit with a `retry-after` — none of which a
//! real endpoint will produce on request, and all of which decide whether
//! the agent behaves when the day is going badly.

use std::collections::VecDeque;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nacelle_ai::backend::anthropic::{self, Anthropic, Effort, HttpResponse, Retry, Transport};
use nacelle_ai::credentials::Credential;
use nacelle_ai::{
    Backend, BackendError, Content, Flow, Message, Request, Role, StopReason, StreamEvent,
    ToolDeclaration,
};
use serde_json::{json, Value};

/// Distinctive on purpose: several tests do nothing but check that this
/// string never comes back out.
const TOKEN: &str = "sk-ant-secret-value-that-must-never-be-logged";

const MODEL: &str = anthropic::DEFAULT_MODEL.id;

// ---------------------------------------------------------------- stubs

/// One canned reply, delivered in `chunk`-sized pieces.
struct Reply {
    status: u16,
    retry_after: Option<Duration>,
    body: String,
    chunk: usize,
}

fn ok(body: String) -> Reply {
    Reply {
        status: 200,
        retry_after: None,
        body,
        chunk: usize::MAX,
    }
}

fn failed(status: u16, body: &str) -> Reply {
    Reply {
        status,
        retry_after: None,
        body: body.to_string(),
        chunk: usize::MAX,
    }
}

/// What the backend sent, kept so a test can look at it.
struct Sent {
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct Stub {
    replies: Mutex<VecDeque<Reply>>,
    sent: Mutex<Vec<Sent>>,
}

impl Stub {
    fn new(replies: Vec<Reply>) -> Arc<Self> {
        Arc::new(Stub {
            replies: Mutex::new(replies.into()),
            sent: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> usize {
        self.sent.lock().unwrap().len()
    }

    /// The body of the first request, as JSON.
    fn body(&self) -> Value {
        let sent = self.sent.lock().unwrap();
        serde_json::from_slice(&sent.first().expect("a request").body).expect("valid JSON")
    }

    fn headers(&self) -> Vec<(String, String)> {
        self.sent
            .lock()
            .unwrap()
            .first()
            .expect("a request")
            .headers
            .clone()
    }

    fn header(&self, name: &str) -> Option<String> {
        self.headers()
            .into_iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
    }
}

/// The backend owns its transport, but a test wants to look at what was
/// sent afterwards. The stub is shared and this is the end the backend
/// holds — a named type because a foreign trait cannot be implemented for
/// `Arc` directly.
struct Handle(Arc<Stub>);

impl Handle {
    fn to(stub: &Arc<Stub>) -> Self {
        Handle(Arc::clone(stub))
    }
}

impl Transport for Handle {
    fn post(
        &self,
        url: &str,
        headers: &[(&'static str, String)],
        body: &[u8],
    ) -> Result<HttpResponse, BackendError> {
        self.0.sent.lock().unwrap().push(Sent {
            url: url.to_string(),
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_string(), value.clone()))
                .collect(),
            body: body.to_vec(),
        });

        let reply = self
            .0
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("the backend asked more times than the test had answers");

        Ok(HttpResponse {
            status: reply.status,
            retry_after: reply.retry_after,
            body: Box::new(Chunked {
                bytes: reply.body.into_bytes(),
                at: 0,
                chunk: reply.chunk,
            }),
        })
    }
}

/// Hands over at most `chunk` bytes per read, so a test can put a frame
/// boundary anywhere it likes.
struct Chunked {
    bytes: Vec<u8>,
    at: usize,
    chunk: usize,
}

impl Read for Chunked {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let take = self.chunk.min(out.len()).min(self.bytes.len() - self.at);
        out[..take].copy_from_slice(&self.bytes[self.at..self.at + take]);
        self.at += take;
        Ok(take)
    }
}

// -------------------------------------------------------------- running

/// Retries with the waiting taken out. The schedule is tested through
/// what gets sent, not by making the suite sit still for it.
fn no_waiting() -> Retry {
    Retry {
        attempts: 3,
        backoff: Duration::ZERO,
        cap: Duration::ZERO,
    }
}

fn turn(
    replies: Vec<Reply>,
    request: &Request,
) -> (Result<(), BackendError>, Vec<StreamEvent>, Arc<Stub>) {
    turn_with(replies, request, Credential::api_key(TOKEN))
}

fn turn_with(
    replies: Vec<Reply>,
    request: &Request,
    credential: Credential,
) -> (Result<(), BackendError>, Vec<StreamEvent>, Arc<Stub>) {
    let stub = Stub::new(replies);
    let mut backend =
        Anthropic::with_transport(credential, Handle::to(&stub)).with_retry(no_waiting());

    let mut events = Vec::new();
    let result = backend.send(request, &mut |event| {
        events.push(event);
        Flow::Continue
    });

    (result, events, stub)
}

fn ask() -> Request {
    Request::new(MODEL).with_message(Message::user("what is the weather?"))
}

// --------------------------------------------------------------- bodies

fn frame(name: &str, data: Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

/// A turn shaped like a real one: text, a tool call whose arguments
/// arrive in pieces, a keep-alive in the middle, and the counts at the
/// end.
fn a_turn_with_a_tool_call() -> String {
    [
        frame(
            "message_start",
            json!({"type": "message_start", "message": {
                "id": "msg_1", "type": "message", "role": "assistant", "model": MODEL,
                "content": [], "stop_reason": Value::Null,
                "usage": {"input_tokens": 25, "cache_creation_input_tokens": 4,
                          "cache_read_input_tokens": 8, "output_tokens": 1}}}),
        ),
        frame(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
                   "content_block": {"type": "text", "text": ""}}),
        ),
        frame(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "Checking "}}),
        ),
        frame("ping", json!({"type": "ping"})),
        frame(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
                   "delta": {"type": "text_delta", "text": "the weather."}}),
        ),
        frame(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        frame(
            "content_block_start",
            json!({"type": "content_block_start", "index": 1,
                   "content_block": {"type": "tool_use", "id": "toolu_1",
                                     "name": "get_weather", "input": {}}}),
        ),
        // Arguments as the provider really sends them: JSON cut wherever
        // the tokeniser happened to land.
        frame(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 1,
                   "delta": {"type": "input_json_delta", "partial_json": "{\"locati"}}),
        ),
        frame(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 1,
                   "delta": {"type": "input_json_delta", "partial_json": "on\": \"Earth\"}"}}),
        ),
        frame(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 1}),
        ),
        frame(
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use",
                   "stop_sequence": Value::Null}, "usage": {"output_tokens": 42}}),
        ),
        frame("message_stop", json!({"type": "message_stop"})),
    ]
    .concat()
}

fn a_plain_turn(stop: &str) -> String {
    [
        frame(
            "message_start",
            json!({"message": {"model": MODEL, "usage": {"input_tokens": 3, "output_tokens": 1}}}),
        ),
        frame(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "text_delta", "text": "hello"}}),
        ),
        frame(
            "message_delta",
            json!({"delta": {"stop_reason": stop}, "usage": {"output_tokens": 5}}),
        ),
        frame("message_stop", json!({})),
    ]
    .concat()
}

fn text_of(events: &[StreamEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Text(fragment) => Some(fragment.as_str()),
            _ => None,
        })
        .collect()
}

fn ended(events: &[StreamEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, StreamEvent::End { .. }))
}

// ----------------------------------------------------------- the stream

#[test]
fn the_reply_is_the_same_however_the_bytes_arrive() {
    // The provider does not decide how the kernel splits its reply, so
    // neither may the parser. One byte per read puts a split inside every
    // field name and every JSON document in the stream.
    let whole = {
        let (result, events, _) = turn(vec![ok(a_turn_with_a_tool_call())], &ask());
        result.expect("stream");
        events
    };

    for chunk in [1, 2, 29, 400] {
        let stub = Stub::new(vec![Reply {
            chunk,
            ..ok(a_turn_with_a_tool_call())
        }]);
        let mut backend = Anthropic::with_transport(Credential::api_key(TOKEN), Handle(stub));
        let mut events = Vec::new();
        backend
            .send(&ask(), &mut |event| {
                events.push(event);
                Flow::Continue
            })
            .expect("stream");

        assert_eq!(events, whole, "read size {chunk} changed the stream");
    }
}

#[test]
fn a_turn_starts_once_ends_once_and_says_what_it_cost() {
    let (result, events, _) = turn(vec![ok(a_turn_with_a_tool_call())], &ask());
    result.expect("stream");

    assert!(matches!(events.first(), Some(StreamEvent::Start { model }) if model == MODEL));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, StreamEvent::End { .. }))
            .count(),
        1
    );
    // The keep-alive is not an event anyone downstream should hear about.
    assert_eq!(text_of(&events), "Checking the weather.");

    let Some(StreamEvent::End { stop, usage }) = events.last() else {
        panic!("no End");
    };
    assert_eq!(*stop, StopReason::ToolUse);
    // Prompt counts come at the start, the completion count at the end,
    // and both have to survive to the same event.
    assert_eq!(usage.input_tokens, 25);
    assert_eq!(usage.output_tokens, 42);
    assert_eq!(usage.cache_read_tokens, 8);
    assert_eq!(usage.cache_write_tokens, 4);
}

#[test]
fn tool_arguments_are_announced_once_and_whole() {
    let (_, events, _) = turn(vec![ok(a_turn_with_a_tool_call())], &ask());

    let calls: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect();

    // One call, not one per fragment: a receiver must never be handed
    // arguments it cannot parse.
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "toolu_1");
    assert_eq!(calls[0].name, "get_weather");
    assert_eq!(calls[0].input, json!({"location": "Earth"}));
}

#[test]
fn a_tool_with_no_arguments_is_an_empty_object_not_a_failure() {
    // Nothing is streamed for a tool that takes nothing, so the buffer
    // is empty at content_block_stop. That is not a truncated document.
    let body = [
        frame("message_start", json!({"message": {"model": MODEL}})),
        frame(
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "tool_use", "id": "t1", "name": "now"}}),
        ),
        frame("content_block_stop", json!({"index": 0})),
        frame(
            "message_delta",
            json!({"delta": {"stop_reason": "tool_use"}}),
        ),
        frame("message_stop", json!({})),
    ]
    .concat();

    let (result, events, _) = turn(vec![ok(body)], &ask());
    result.expect("stream");

    assert!(events
        .iter()
        .any(|event| matches!(event, StreamEvent::ToolCall(call) if call.input == json!({}))));
}

#[test]
fn unparsable_tool_arguments_are_a_protocol_failure() {
    let body = [
        frame("message_start", json!({"message": {"model": MODEL}})),
        frame(
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "tool_use", "id": "t1", "name": "now"}}),
        ),
        frame(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"a\": "}}),
        ),
        frame("content_block_stop", json!({"index": 0})),
    ]
    .concat();

    let (result, events, _) = turn(vec![ok(body)], &ask());

    assert!(matches!(result, Err(BackendError::Protocol(_))));
    assert!(!ended(&events));
}

#[test]
fn stop_reasons_arrive_in_the_contract_s_own_terms() {
    for (provider, expected) in [
        ("end_turn", StopReason::EndTurn),
        ("tool_use", StopReason::ToolUse),
        ("max_tokens", StopReason::MaxTokens),
        ("stop_sequence", StopReason::StopSequence),
    ] {
        let (result, events, _) = turn(vec![ok(a_plain_turn(provider))], &ask());
        result.expect("stream");
        assert!(
            matches!(events.last(), Some(StreamEvent::End { stop, .. }) if *stop == expected),
            "{provider}"
        );
    }
}

#[test]
fn a_pause_is_carried_through_rather_than_flattened() {
    // The model ran out of room mid-task. It is not an ending, and the
    // caller has to be able to tell — it resends the conversation so the
    // model can finish.
    let (result, events, _) = turn(vec![ok(a_plain_turn(anthropic::STOP_PAUSE_TURN))], &ask());
    result.expect("stream");

    let Some(StreamEvent::End { stop, .. }) = events.last() else {
        panic!("no End");
    };
    assert_eq!(
        *stop,
        StopReason::Other(anthropic::STOP_PAUSE_TURN.to_string())
    );
}

#[test]
fn a_refusal_is_a_failure_and_never_an_ending() {
    // Reported as HTTP 200 with a stop reason, which is exactly why it
    // has to be turned into an error here: a caller that saw End would
    // treat a refused turn as a finished one. Note the half-written
    // answer before it, and the null stop_details — both happen.
    let body = [
        frame("message_start", json!({"message": {"model": MODEL}})),
        frame(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "text_delta", "text": "I can help with"}}),
        ),
        frame(
            "message_delta",
            json!({"delta": {"stop_reason": "refusal", "stop_details": Value::Null}}),
        ),
        frame("message_stop", json!({})),
    ]
    .concat();

    let (result, events, _) = turn(vec![ok(body)], &ask());

    assert!(matches!(result, Err(BackendError::Refused { .. })));
    assert!(!ended(&events), "a refused turn must not look finished");
    assert!(!result.unwrap_err().is_retryable());
}

#[test]
fn a_refusal_keeps_whatever_the_provider_said_about_it() {
    let body = [
        frame("message_start", json!({"message": {"model": MODEL}})),
        frame(
            "message_delta",
            json!({"delta": {"stop_reason": "refusal",
                   "stop_details": {"type": "refusal", "category": "cyber",
                                    "explanation": "declined by policy"}}}),
        ),
    ]
    .concat();

    let (result, _, _) = turn(vec![ok(body)], &ask());

    assert_eq!(
        result,
        Err(BackendError::Refused {
            category: Some("cyber".to_string()),
            explanation: Some("declined by policy".to_string()),
        })
    );
}

#[test]
fn an_error_in_the_middle_of_a_reply_ends_it_without_an_end() {
    let body = [
        frame("message_start", json!({"message": {"model": MODEL}})),
        frame(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "text_delta", "text": "half an ans"}}),
        ),
        frame(
            "error",
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
        ),
    ]
    .concat();

    let (result, events, _) = turn(vec![ok(body)], &ask());

    assert_eq!(
        result,
        Err(BackendError::Server {
            status: 529,
            message: "Overloaded".to_string()
        })
    );
    // Retryable as a fact about the error — but see the retry tests: not
    // once the receiver has already seen part of an answer.
    assert!(BackendError::Server {
        status: 529,
        message: String::new()
    }
    .is_retryable());
    assert!(!ended(&events));
    assert_eq!(text_of(&events), "half an ans");
}

#[test]
fn a_reply_that_stops_early_is_a_failure() {
    // The connection dropped after some text. Whatever arrived is a
    // fragment, and calling it an answer would be the one unforgivable
    // bug in a streaming client.
    let truncated = a_turn_with_a_tool_call();
    let cut = truncated.len() / 2;

    let (result, events, _) = turn(vec![ok(truncated[..cut].to_string())], &ask());

    assert!(matches!(result, Err(BackendError::Protocol(_))));
    assert!(!ended(&events));
}

#[test]
fn stopping_the_sink_cancels_the_turn() {
    let stub = Stub::new(vec![ok(a_turn_with_a_tool_call())]);
    let mut backend = Anthropic::with_transport(Credential::api_key(TOKEN), Handle(stub));

    let mut events = Vec::new();
    let result = backend.send(&ask(), &mut |event| {
        let first_text = matches!(event, StreamEvent::Text(_));
        events.push(event);
        if first_text {
            Flow::Stop
        } else {
            Flow::Continue
        }
    });

    assert_eq!(result, Err(BackendError::Cancelled));
    assert!(!ended(&events));
}

// ---------------------------------------------------------- the request

#[test]
fn the_request_says_what_these_models_need_to_hear() {
    let request = ask()
        .with_system("you are a desktop agent")
        .with_max_tokens(anthropic::RECOMMENDED_MAX_TOKENS)
        .with_thinking(true)
        .with_tool(ToolDeclaration::new(
            "get_weather",
            "Look up the weather. Call it when asked about conditions somewhere.",
            json!({"type": "object", "properties": {"location": {"type": "string"}}}),
        ));

    let (_, _, stub) = turn(vec![ok(a_plain_turn("end_turn"))], &request);
    let body = stub.body();

    assert_eq!(body["model"], json!(MODEL));
    assert_eq!(body["max_tokens"], json!(64_000));
    assert_eq!(body["stream"], json!(true));
    // Asked for explicitly: leaving the field out means no thinking at
    // all on these models, and the summary is what makes progress
    // visible instead of a long silence.
    assert_eq!(
        body["thinking"],
        json!({"type": "adaptive", "display": "summarized"})
    );
    assert_eq!(body["output_config"], json!({"effort": "high"}));

    // The system prompt is a block list so it can carry the cache
    // breakpoint. That is the whole reason it is not a bare string.
    assert_eq!(body["system"][0]["text"], json!("you are a desktop agent"));
    assert_eq!(
        body["system"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );

    assert_eq!(body["tools"][0]["name"], json!("get_weather"));
    assert_eq!(
        body["tools"][0]["input_schema"]["properties"]["location"]["type"],
        json!("string")
    );
    assert_eq!(stub.requests(), 1);
}

#[test]
fn the_parameters_these_models_reject_are_never_sent() {
    // Each of these is a 400 on Opus 4.8, and each is what a reader who
    // learned the API a year ago would expect to find here.
    let request = ask().with_thinking(true);
    let (_, _, stub) = turn(vec![ok(a_plain_turn("end_turn"))], &request);
    let body = stub.body();

    for banned in ["temperature", "top_p", "top_k"] {
        assert!(body.get(banned).is_none(), "{banned} was sent");
    }
    assert!(body["thinking"].get("budget_tokens").is_none());
}

#[test]
fn thinking_is_left_out_unless_it_was_asked_for() {
    // Not `{"type": "disabled"}`: leaving the field out is what every one
    // of these models reads as "no thinking", and one of them rejects
    // being told so explicitly.
    let (_, _, stub) = turn(vec![ok(a_plain_turn("end_turn"))], &ask());
    assert!(stub.body().get("thinking").is_none());
}

#[test]
fn effort_is_the_dial_and_it_goes_out_with_every_request() {
    let stub = Stub::new(vec![ok(a_plain_turn("end_turn"))]);
    let mut backend = Anthropic::with_transport(Credential::api_key(TOKEN), Handle::to(&stub))
        .with_effort(Effort::XHigh)
        .with_thinking_summary(false);

    backend
        .send(&ask().with_thinking(true), &mut |_| Flow::Continue)
        .expect("stream");

    let body = stub.body();
    // No separator in "xhigh" — easy to get wrong, rejected when you do.
    assert_eq!(body["output_config"]["effort"], json!("xhigh"));
    assert_eq!(body["thinking"], json!({"type": "adaptive"}));
}

#[test]
fn every_tool_result_rides_in_the_one_user_message() {
    // The endpoint requires one tool_result per tool_use, all in a single
    // user turn, failures included and marked. Splitting them across
    // messages is rejected, and silently dropping the failed one is worse
    // than being rejected.
    let request = ask()
        .with_message(Message::new(
            Role::Assistant,
            vec![
                Content::Text("checking".to_string()),
                Content::ToolUse(nacelle_ai::ToolCall {
                    id: "toolu_1".to_string(),
                    name: "get_weather".to_string(),
                    input: json!({"location": "Earth"}),
                }),
            ],
        ))
        .with_message(Message::new(
            Role::User,
            vec![
                Content::ToolResult {
                    id: "toolu_1".to_string(),
                    output: "17C".to_string(),
                    is_error: false,
                },
                Content::ToolResult {
                    id: "toolu_2".to_string(),
                    output: "no such place".to_string(),
                    is_error: true,
                },
            ],
        ));

    let (_, _, stub) = turn(vec![ok(a_plain_turn("end_turn"))], &request);
    let body = stub.body();
    let last = &body["messages"][2];

    assert_eq!(last["role"], json!("user"));
    assert_eq!(last["content"].as_array().expect("blocks").len(), 2);
    assert_eq!(last["content"][0]["tool_use_id"], json!("toolu_1"));
    // Said only when it is true: telling the model every successful call
    // did not fail is noise in its context.
    assert!(last["content"][0].get("is_error").is_none());
    assert_eq!(last["content"][1]["is_error"], json!(true));

    let call = &body["messages"][1]["content"][1];
    assert_eq!(call["type"], json!("tool_use"));
    assert_eq!(call["input"], json!({"location": "Earth"}));
}

#[test]
fn a_leading_system_message_becomes_the_system_prompt() {
    // The endpoint refuses a system message in first position, so a
    // leading one can only ever have meant the system prompt. A later one
    // is a mid-conversation instruction and stays where it was put.
    let request = Request::new(MODEL)
        .with_message(Message::system("stay terse"))
        .with_message(Message::user("hello"))
        .with_message(Message::system("the user switched to Polish"));

    let (_, _, stub) = turn(vec![ok(a_plain_turn("end_turn"))], &request);
    let body = stub.body();

    assert_eq!(body["system"][0]["text"], json!("stay terse"));
    assert_eq!(body["messages"].as_array().expect("messages").len(), 2);
    assert_eq!(body["messages"][0]["role"], json!("user"));
    assert_eq!(body["messages"][1]["role"], json!("system"));
}

#[test]
fn an_unsigned_thinking_block_is_dropped_rather_than_sent() {
    // The endpoint verifies the signature and rejects a thinking block
    // that was edited or rebuilt. Dropping it costs the model a little
    // context; sending it costs the whole turn.
    let request = ask().with_message(Message::new(
        Role::Assistant,
        vec![
            Content::Thinking {
                text: "unsigned".to_string(),
                signature: None,
            },
            Content::Thinking {
                text: "signed".to_string(),
                signature: Some("sig-1".to_string()),
            },
        ],
    ));

    let (_, _, stub) = turn(vec![ok(a_plain_turn("end_turn"))], &request);
    let blocks = stub.body()["messages"][1]["content"].clone();

    assert_eq!(blocks.as_array().expect("blocks").len(), 1);
    assert_eq!(blocks[0]["thinking"], json!("signed"));
    assert_eq!(blocks[0]["signature"], json!("sig-1"));
}

// ---------------------------------------------------------- the headers

#[test]
fn an_api_key_and_an_oauth_token_are_carried_differently() {
    // The classic trap: these differ by *header name*, not by value.
    // Sending an OAuth token as x-api-key is a 401 that explains nothing.
    let (_, _, key) = turn_with(
        vec![ok(a_plain_turn("end_turn"))],
        &ask(),
        Credential::api_key(TOKEN),
    );
    assert_eq!(key.header("x-api-key").as_deref(), Some(TOKEN));
    assert!(key.header("authorization").is_none());
    assert!(key.header("anthropic-beta").is_none());

    let (_, _, oauth) = turn_with(
        vec![ok(a_plain_turn("end_turn"))],
        &ask(),
        Credential::oauth(TOKEN),
    );
    assert_eq!(
        oauth.header("authorization").as_deref(),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert!(oauth.header("x-api-key").is_none());
    // An OAuth token is only accepted alongside its beta flag.
    assert_eq!(
        oauth.header("anthropic-beta").as_deref(),
        Some("oauth-2025-04-20")
    );
}

#[test]
fn every_request_pins_the_wire_format() {
    let (_, _, stub) = turn(vec![ok(a_plain_turn("end_turn"))], &ask());

    assert_eq!(
        stub.header("anthropic-version").as_deref(),
        Some("2023-06-01")
    );
    assert_eq!(
        stub.header("content-type").as_deref(),
        Some("application/json")
    );
    assert_eq!(
        stub.sent.lock().unwrap()[0].url,
        "https://api.anthropic.com/v1/messages"
    );
}

#[test]
fn beta_flags_are_merged_into_the_one_header() {
    // A second header of the same name would be ignored: the endpoint
    // reads one. So the flag an OAuth credential brings and the flags a
    // backend adds have to end up in the same value.
    let stub = Stub::new(vec![ok(a_plain_turn("end_turn"))]);
    let mut backend = Anthropic::with_transport(Credential::oauth(TOKEN), Handle::to(&stub))
        .with_beta("fine-grained-tool-streaming-2025-05-14")
        .with_beta("token-efficient-tools-2024-11-01");

    backend
        .send(&ask(), &mut |_| Flow::Continue)
        .expect("stream");

    let betas: Vec<_> = stub
        .headers()
        .into_iter()
        .filter(|(name, _)| name == "anthropic-beta")
        .collect();

    assert_eq!(betas.len(), 1, "one header, whatever the flag count");
    assert_eq!(
        betas[0].1,
        "oauth-2025-04-20,fine-grained-tool-streaming-2025-05-14,token-efficient-tools-2024-11-01"
    );
}

// -------------------------------------------------------------- failure

#[test]
fn a_rejected_credential_is_not_worth_retrying() {
    let (result, _, stub) = turn(
        vec![failed(
            401,
            r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
        )],
        &ask(),
    );

    let err = result.expect_err("401");
    assert!(matches!(err, BackendError::Auth(_)));
    assert!(!err.is_retryable());
    assert_eq!(
        stub.requests(),
        1,
        "asking again with the same token is pointless"
    );
    // The provider's own words, so the user knows what to fix.
    assert!(err.to_string().contains("invalid x-api-key"));
}

#[test]
fn a_malformed_request_says_what_was_wrong_with_it() {
    let (result, _, stub) = turn(
        vec![failed(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: must be >= 1"}}"#,
        )],
        &ask(),
    );

    let err = result.expect_err("400");
    // Wrong once, wrong twice: the request will not fix itself.
    assert!(!err.is_retryable());
    assert_eq!(stub.requests(), 1);
    assert!(err.to_string().contains("max_tokens: must be >= 1"));
    assert!(err.to_string().contains("400"));
}

#[test]
fn an_answer_that_is_not_json_is_still_passed_on() {
    // A proxy or a gateway answered instead of the provider. Whatever it
    // said is the only clue there is.
    let (result, _, _) = turn(
        vec![failed(
            413,
            "<html><body>Request Entity Too Large</body></html>",
        )],
        &ask(),
    );

    let err = result.expect_err("413");
    assert!(!err.is_retryable());
    assert!(err.to_string().contains("Request Entity Too Large"));
}

#[test]
fn a_provider_failure_is_tried_again() {
    let (result, events, stub) = turn(
        vec![
            failed(
                529,
                r#"{"error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            ),
            ok(a_plain_turn("end_turn")),
        ],
        &ask(),
    );

    result.expect("the second attempt");
    assert_eq!(stub.requests(), 2);
    // And the receiver saw one clean turn, not the wreckage of the first.
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, StreamEvent::Start { .. }))
            .count(),
        1
    );
}

#[test]
fn retrying_gives_up_after_the_agreed_number_of_attempts() {
    let overloaded = || {
        failed(
            500,
            r#"{"error":{"type":"api_error","message":"internal"}}"#,
        )
    };
    let (result, _, stub) = turn(vec![overloaded(), overloaded(), overloaded()], &ask());

    assert!(result.is_err());
    assert_eq!(stub.requests(), 3);
}

#[test]
fn a_turn_that_already_spoke_is_never_retried() {
    // This is the rule that keeps the contract honest. The receiver has
    // seen a Start and some text; a second attempt would send it a second
    // Start and repeat the text, and exactly one Start per turn is the
    // whole promise.
    let broke_mid_stream = [
        frame("message_start", json!({"message": {"model": MODEL}})),
        frame(
            "content_block_delta",
            json!({"index": 0, "delta": {"type": "text_delta", "text": "half"}}),
        ),
        frame(
            "error",
            json!({"error": {"type": "overloaded_error", "message": "Overloaded"}}),
        ),
    ]
    .concat();

    let (result, events, stub) = turn(
        vec![ok(broke_mid_stream), ok(a_plain_turn("end_turn"))],
        &ask(),
    );

    assert!(result.is_err());
    assert_eq!(
        stub.requests(),
        1,
        "the failure was retryable, the turn was not"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, StreamEvent::Start { .. }))
            .count(),
        1
    );
}

#[test]
fn a_rate_limit_carries_the_provider_s_own_delay() {
    let stub = Stub::new(vec![Reply {
        retry_after: Some(Duration::from_secs(17)),
        ..failed(
            429,
            r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#,
        )
    }]);
    // One attempt, so the delay is reported rather than slept through.
    let mut backend = Anthropic::with_transport(Credential::api_key(TOKEN), Handle(stub))
        .with_retry(Retry {
            attempts: 1,
            ..no_waiting()
        });

    let err = backend
        .send(&ask(), &mut |_| Flow::Continue)
        .expect_err("429");

    assert!(err.is_retryable());
    assert_eq!(err.retry_after(), Some(Duration::from_secs(17)));
}

#[test]
fn no_failure_anywhere_carries_the_credential() {
    // Errors are the most likely thing to be logged, mailed or pasted
    // into a bug report. The token is in a header, and no error is built
    // out of headers — this is the test that says so for every path at
    // once.
    let failures = vec![
        failed(401, r#"{"error":{"type":"authentication_error","message":"bad key"}}"#),
        failed(400, "not json at all"),
        failed(429, r#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#),
        failed(500, r#"{"error":{"type":"api_error","message":"internal"}}"#),
        ok("event: error\ndata: {\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}\n\n"
            .to_string()),
        ok("event: message_start\ndata: nonsense\n\n".to_string()),
        ok(String::new()),
    ];

    for reply in failures {
        let status = reply.status;
        for credential in [Credential::api_key(TOKEN), Credential::oauth(TOKEN)] {
            let stub = Stub::new(vec![Reply { ..reply_of(&reply) }]);
            let mut backend =
                Anthropic::with_transport(credential, Handle(stub)).with_retry(Retry {
                    attempts: 1,
                    ..no_waiting()
                });

            let err = backend
                .send(&ask(), &mut |_| Flow::Continue)
                .expect_err("a failure");

            let shown = format!("{err} {err:?}");
            assert!(!shown.contains(TOKEN), "status {status} leaked the token");
            assert!(!shown.contains("Bearer"), "status {status} leaked a header");
        }
    }
}

/// `Reply` is not `Clone` because a body is consumed as it is read; this
/// copies one for a second run.
fn reply_of(reply: &Reply) -> Reply {
    Reply {
        status: reply.status,
        retry_after: reply.retry_after,
        body: reply.body.clone(),
        chunk: reply.chunk,
    }
}

// --------------------------------------------------------------- models

#[test]
fn the_models_offered_are_named_exactly_as_the_endpoint_wants_them() {
    // A date suffix appended to any of these is a 404, not a newer model.
    assert_eq!(anthropic::DEFAULT_MODEL.id, "claude-opus-4-8");
    assert_eq!(anthropic::OPUS_4_8.id, "claude-opus-4-8");
    assert_eq!(anthropic::SONNET_5.id, "claude-sonnet-5");
    assert_eq!(anthropic::HAIKU_4_5.id, "claude-haiku-4-5");

    for model in anthropic::MODELS {
        assert!(!model.summary.is_empty(), "{} has no description", model.id);
        assert!(
            !model.id.contains("-2024")
                && !model.id.contains("-2025")
                && !model.id.contains("-2026"),
            "{} looks like it grew a date",
            model.id
        );
    }
}

#[test]
fn the_backend_names_itself_and_nothing_else() {
    let backend = Anthropic::with_transport(Credential::api_key(TOKEN), Handle(Stub::new(vec![])));
    // Never a URL, never anything derived from the credential.
    assert_eq!(backend.name(), "anthropic");

    // Object safety is a requirement: which provider answers is a
    // configuration value, so the agent loop holds a boxed trait object.
    let boxed: Box<dyn Backend> = Box::new(backend);
    assert_eq!(boxed.name(), "anthropic");
}
