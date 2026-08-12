//! The Ollama backend, against recorded bytes.
//!
//! Not one of these tests opens a socket. Everything the backend has to
//! get right is a property of two byte strings — the body it sends and
//! the body it reads — so a running server would prove nothing here that
//! a recording does not, while making the suite depend on which models
//! happen to be pulled on the machine running it.
//!
//! The recordings are shaped like real `/api/chat` traffic: one JSON
//! object per line, the increments in `message.content`, a final object
//! with `done: true` and the counters.

use std::collections::HashMap;
use std::io::Read;
use std::time::Duration;

use nacelle_ai::backend::ollama::{self, Ollama};
use nacelle_ai::{
    Backend, BackendError, Content, Flow, Message, Request, Role, StopReason, StreamEvent,
    ToolCall, ToolDeclaration,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------- tools

/// A reader that gives up `chunk` bytes at a time.
///
/// The point of the whole exercise: a real socket returns whatever has
/// arrived, which is regularly half a line and occasionally half a
/// character. A parser that only ever sees whole lines in its tests has
/// not been tested.
struct Trickle {
    bytes: Vec<u8>,
    at: usize,
    chunk: usize,
}

impl Trickle {
    fn new(text: &str, chunk: usize) -> Self {
        Trickle {
            bytes: text.as_bytes().to_vec(),
            at: 0,
            chunk,
        }
    }
}

impl Read for Trickle {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let left = self.bytes.len() - self.at;
        let take = self.chunk.min(out.len()).min(left);
        out[..take].copy_from_slice(&self.bytes[self.at..self.at + take]);
        self.at += take;
        Ok(take)
    }
}

fn collect(body: &str) -> Result<Vec<StreamEvent>, BackendError> {
    collect_in_chunks(body, body.len().max(1))
}

fn collect_in_chunks(body: &str, chunk: usize) -> Result<Vec<StreamEvent>, BackendError> {
    let mut events = Vec::new();
    ollama::translate_stream(Trickle::new(body, chunk), "asked-for", &mut |event| {
        events.push(event);
        Flow::Continue
    })?;
    Ok(events)
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

fn tool_calls_of(events: &[StreamEvent]) -> Vec<&ToolCall> {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect()
}

// ------------------------------------------------------------ recordings

/// A plain answer in three increments, then the counters.
const PROSE: &str = concat!(
    r#"{"model":"gemma:latest","created_at":"2026-01-01T00:00:00Z","message":{"role":"assistant","content":"The "},"done":false}"#,
    "\n",
    r#"{"model":"gemma:latest","created_at":"2026-01-01T00:00:01Z","message":{"role":"assistant","content":"nacelle "},"done":false}"#,
    "\n",
    r#"{"model":"gemma:latest","created_at":"2026-01-01T00:00:02Z","message":{"role":"assistant","content":"is warm."},"done":false}"#,
    "\n",
    r#"{"model":"gemma:latest","created_at":"2026-01-01T00:00:03Z","message":{"role":"assistant","content":""},"done_reason":"stop","done":true,"prompt_eval_count":31,"eval_count":9}"#,
    "\n",
);

/// The same turn, but the model asks for a tool. The call arrives whole,
/// as Ollama sends it, and there is no id anywhere in it.
const TOOL_TURN: &str = concat!(
    r#"{"model":"nemotron-3-nano:30b","message":{"role":"assistant","content":"Checking the weather."},"done":false}"#,
    "\n",
    r#"{"model":"nemotron-3-nano:30b","message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"get_weather","arguments":{"location":"Earth","unit":"celsius"}}}]},"done":false}"#,
    "\n",
    r#"{"model":"nemotron-3-nano:30b","message":{"role":"assistant","content":""},"done_reason":"stop","done":true,"prompt_eval_count":52,"eval_count":21}"#,
    "\n",
);

const TAGS: &str = r#"{
  "models": [
    {
      "name": "gemma4:31b-it-q4_K_M",
      "model": "gemma4:31b-it-q4_K_M",
      "modified_at": "2026-01-01T00:00:00Z",
      "size": 18600000000,
      "digest": "aaa",
      "details": {
        "family": "gemma4",
        "parameter_size": "31.0B",
        "quantization_level": "Q4_K_M"
      }
    },
    {
      "name": "gemma:latest",
      "size": 5400000000,
      "details": { "family": "gemma" }
    }
  ]
}"#;

// ------------------------------------------------------- reading a turn

#[test]
fn a_turn_becomes_start_text_and_one_end() {
    let events = collect(PROSE).expect("a well-formed turn");

    assert_eq!(
        events.first(),
        Some(&StreamEvent::Start {
            model: "gemma:latest".to_string()
        }),
        "the model the server names, not the one that was asked for"
    );
    assert_eq!(text_of(&events), "The nacelle is warm.");
    assert!(matches!(events.last(), Some(StreamEvent::End { .. })));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, StreamEvent::End { .. }))
            .count(),
        1
    );

    match events.last() {
        Some(StreamEvent::End { stop, usage }) => {
            assert_eq!(*stop, StopReason::EndTurn);
            assert_eq!(usage.input_tokens, 31);
            assert_eq!(usage.output_tokens, 9);
            // Ollama reports no cache counters; zero is "not said".
            assert_eq!(usage.cache_read_tokens, 0);
            assert_eq!(usage.total_tokens(), 40);
        }
        other => panic!("expected End, got {other:?}"),
    }
}

#[test]
fn an_object_split_across_reads_is_reassembled() {
    // One byte per read is the worst a socket can do, and the answer has
    // to come out the same as when the whole body arrives at once.
    let whole = collect_in_chunks(PROSE, PROSE.len()).expect("one read");
    let trickled = collect_in_chunks(PROSE, 1).expect("one byte at a time");
    let awkward = collect_in_chunks(PROSE, 7).expect("seven bytes at a time");

    assert_eq!(whole, trickled);
    assert_eq!(whole, awkward);
}

#[test]
fn blank_lines_are_framing_and_not_events() {
    let padded = format!("\n\n{PROSE}\n   \n\n");
    let events = collect(&padded).expect("blank lines are skipped");

    assert_eq!(events, collect(PROSE).expect("the same turn unpadded"));
}

#[test]
fn the_model_is_named_even_when_the_server_does_not() {
    let body = "{\"message\":{\"content\":\"hi\"},\"done\":true,\"done_reason\":\"stop\"}\n";
    let events = collect(body).expect("a turn without a model field");

    assert_eq!(
        events.first(),
        Some(&StreamEvent::Start {
            model: "asked-for".to_string()
        })
    );
}

#[test]
fn thinking_is_its_own_kind_of_fragment() {
    let body = concat!(
        r#"{"model":"nemotron-3-nano:30b","message":{"role":"assistant","thinking":"Weighing it up.","content":""},"done":false}"#,
        "\n",
        r#"{"model":"nemotron-3-nano:30b","message":{"role":"assistant","content":"Yes."},"done":true,"done_reason":"stop"}"#,
        "\n",
    );
    let events = collect(body).expect("a thinking turn");

    assert!(events.contains(&StreamEvent::Thinking("Weighing it up.".to_string())));
    assert_eq!(text_of(&events), "Yes.", "reasoning is not the answer");
}

#[test]
fn the_token_limit_is_not_a_finished_answer() {
    let body = r#"{"model":"gemma:latest","message":{"content":"half a th"},"done":true,"done_reason":"length"}"#;
    let events = collect(body).expect("a truncated-by-limit turn");

    match events.last() {
        Some(StreamEvent::End { stop, .. }) => assert_eq!(*stop, StopReason::MaxTokens),
        other => panic!("expected End, got {other:?}"),
    }
}

// --------------------------------------------------------- tool calling

#[test]
fn a_tool_call_arrives_whole_and_is_announced_once() {
    let events = collect(TOOL_TURN).expect("a tool turn");
    let calls = tool_calls_of(&events);

    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "get_weather");
    assert_eq!(
        calls[0].input,
        json!({"location": "Earth", "unit": "celsius"})
    );
    assert!(
        !calls[0].id.is_empty(),
        "Ollama sends no id, so the backend has to mint one — the result \
         has nothing else to be matched by"
    );
}

#[test]
fn a_turn_that_ends_in_a_tool_call_says_so() {
    // Ollama calls this an ordinary stop. The contract does not: the
    // caller now owes the model a result, and a caller that had to know
    // which provider answered to work that out would be reading the
    // wrong enum.
    let events = collect(TOOL_TURN).expect("a tool turn");

    match events.last() {
        Some(StreamEvent::End { stop, .. }) => assert_eq!(*stop, StopReason::ToolUse),
        other => panic!("expected End, got {other:?}"),
    }
}

#[test]
fn an_id_from_the_server_is_kept_verbatim() {
    // Recent servers do identify their calls. Theirs wins: an id is the
    // caller's to echo back, so it is carried and never interpreted, and
    // minting one over the top would break a server that matches by it.
    let body = concat!(
        r#"{"model":"m","message":{"tool_calls":[{"id":"call_5s2rfpgg","function":{"name":"get_weather","arguments":{"location":"Warsaw"}}}]},"done":false}"#,
        "\n",
        r#"{"model":"m","message":{"content":""},"done":true,"done_reason":"stop"}"#,
        "\n",
    );
    let events = collect(body).expect("a turn from a server that sends ids");

    assert_eq!(tool_calls_of(&events)[0].id, "call_5s2rfpgg");
}

#[test]
fn an_empty_id_is_no_id() {
    // A field that is present and blank is the same as absent, and an
    // empty id would make two calls in one turn indistinguishable.
    let body = concat!(
        r#"{"model":"m","message":{"tool_calls":[{"id":"","function":{"name":"a"}},{"id":"","function":{"name":"b"}}]},"done":true,"done_reason":"stop"}"#,
        "\n",
    );
    let events = collect(body).expect("a turn with blank ids");
    let calls = tool_calls_of(&events);

    assert!(!calls[0].id.is_empty());
    assert_ne!(calls[0].id, calls[1].id);
}

#[test]
fn arguments_sent_as_a_json_string_are_still_arguments() {
    let body = concat!(
        r#"{"model":"m","message":{"tool_calls":[{"function":{"name":"open","arguments":"{\"path\":\"/etc\"}"}}]},"done":false}"#,
        "\n",
        r#"{"model":"m","message":{"content":""},"done":true,"done_reason":"stop"}"#,
        "\n",
    );
    let events = collect(body).expect("a stringified-arguments turn");

    assert_eq!(tool_calls_of(&events)[0].input, json!({"path": "/etc"}));
}

#[test]
fn arguments_that_are_not_json_are_a_protocol_error() {
    let body = r#"{"model":"m","message":{"tool_calls":[{"function":{"name":"open","arguments":"{path:"}}]},"done":false}"#;
    let error = collect(body).expect_err("half a document is not a document");

    assert!(matches!(error, BackendError::Protocol(_)), "got {error:?}");
    assert!(error.to_string().contains("open"), "name the tool");
    assert!(!error.is_retryable());
}

#[test]
fn a_tool_that_takes_no_arguments_still_makes_a_call() {
    let body = concat!(
        r#"{"model":"m","message":{"tool_calls":[{"function":{"name":"uptime"}}]},"done":false}"#,
        "\n",
        r#"{"model":"m","message":{"content":""},"done":true,"done_reason":"stop"}"#,
        "\n",
    );
    let events = collect(body).expect("an argumentless call");

    assert_eq!(tool_calls_of(&events)[0].input, json!({}));
}

#[test]
fn minted_ids_do_not_collide_between_tools() {
    let body = concat!(
        r#"{"model":"m","message":{"tool_calls":[{"function":{"name":"a","arguments":{}}},{"function":{"name":"b","arguments":{}}}]},"done":false}"#,
        "\n",
        r#"{"model":"m","message":{"content":""},"done":true,"done_reason":"stop"}"#,
        "\n",
    );
    let events = collect(body).expect("two calls in one object");
    let calls = tool_calls_of(&events);

    assert_eq!(calls.len(), 2);
    assert_ne!(calls[0].id, calls[1].id);
}

// ------------------------------------------------------------- failures

#[test]
fn a_stream_that_stops_in_the_middle_is_an_error_and_not_an_end() {
    // The body ends after two increments: no `done`, so no answer. The
    // caller must not be able to read this as a short reply.
    let truncated = PROSE.lines().take(2).collect::<Vec<_>>().join("\n");
    let mut events = Vec::new();
    let error = ollama::translate_stream(truncated.as_bytes(), "asked-for", &mut |event| {
        events.push(event);
        Flow::Continue
    })
    .expect_err("a turn with no end is not a turn");

    assert!(matches!(error, BackendError::Network(_)), "got {error:?}");
    assert!(
        error.is_retryable(),
        "a dropped connection is worth retrying"
    );
    assert!(!events
        .iter()
        .any(|event| matches!(event, StreamEvent::End { .. })));
}

#[test]
fn a_body_that_stops_mid_object_is_a_dropped_connection_not_a_bad_server() {
    let half = r#"{"model":"m","message":{"content":"star"#;
    let error = collect(half).expect_err("half an object");

    assert!(
        matches!(error, BackendError::Network(_)),
        "a truncated object is a transport failure, not a protocol one: got {error:?}"
    );
}

#[test]
fn a_line_that_is_not_json_is_a_protocol_error() {
    let error = collect("this is not JSON at all\n").expect_err("nonsense");

    assert!(matches!(error, BackendError::Protocol(_)), "got {error:?}");
}

#[test]
fn an_error_object_mid_stream_ends_the_turn_without_ending_it() {
    let body = concat!(
        r#"{"model":"m","message":{"content":"start"},"done":false}"#,
        "\n",
        r#"{"error":"model runner has terminated"}"#,
        "\n",
    );
    let mut events = Vec::new();
    let error = ollama::translate_stream(body.as_bytes(), "m", &mut |event| {
        events.push(event);
        Flow::Continue
    })
    .expect_err("an error object is a failure");

    assert!(error.to_string().contains("model runner has terminated"));
    assert!(!events
        .iter()
        .any(|event| matches!(event, StreamEvent::End { .. })));
}

#[test]
fn stopping_the_sink_cancels_without_ending() {
    let mut events = Vec::new();
    let error = ollama::translate_stream(PROSE.as_bytes(), "asked-for", &mut |event| {
        let first_text = matches!(event, StreamEvent::Text(_));
        events.push(event);
        if first_text {
            Flow::Stop
        } else {
            Flow::Continue
        }
    })
    .expect_err("a stopped sink cancels the turn");

    assert_eq!(error, BackendError::Cancelled);
    assert!(!events
        .iter()
        .any(|event| matches!(event, StreamEvent::End { .. })));
}

#[test]
fn a_missing_model_says_how_to_get_it() {
    let error = ollama::http_error(
        404,
        None,
        r#"{"error":"model \"gemma9:latest\" not found, try pulling it first"}"#,
    );

    let said = error.to_string();
    assert!(said.contains("gemma9:latest"), "{said}");
    assert!(said.contains("not installed"), "{said}");
    assert!(
        !error.is_retryable(),
        "pulling is the user's move, not a retry"
    );
}

#[test]
fn a_model_without_tool_support_is_told_outright() {
    let error = ollama::http_error(
        400,
        None,
        r#"{"error":"registry.ollama.ai/library/gemma:latest does not support tools"}"#,
    );

    let said = error.to_string();
    assert!(said.contains("cannot use tools"), "{said}");
    assert!(
        said.contains("pick a model that supports them"),
        "an error that does not say what to do next is half an error: {said}"
    );
    assert!(!error.is_retryable());
}

#[test]
fn a_model_without_thinking_support_is_told_outright() {
    let error = ollama::http_error(
        400,
        None,
        r#"{"error":"gemma:latest does not support thinking"}"#,
    );

    assert!(error.to_string().contains("thinking turned off"));
}

#[test]
fn statuses_map_onto_the_reactions_they_call_for() {
    assert!(matches!(
        ollama::http_error(401, None, "{}"),
        BackendError::Auth(_)
    ));
    assert!(matches!(
        ollama::http_error(500, None, r#"{"error":"out of memory"}"#),
        BackendError::Server { status: 500, .. }
    ));
    assert!(ollama::http_error(500, None, "{}").is_retryable());
    assert!(!ollama::http_error(400, None, "{}").is_retryable());

    let limited = ollama::http_error(429, Some(Duration::from_secs(12)), "{}");
    assert_eq!(limited.retry_after(), Some(Duration::from_secs(12)));
}

#[test]
fn a_body_that_is_not_json_is_still_reported() {
    // A proxy in front of Ollama answers in HTML. The status is all
    // there is, and dropping the body entirely would leave a bare number.
    let error = ollama::http_error(502, None, "<html>Bad Gateway</html>");

    assert!(error.to_string().contains("Bad Gateway"));
    assert!(error.is_retryable());
}

// -------------------------------------------------------- what we send

fn tool() -> ToolDeclaration {
    ToolDeclaration::new(
        "get_weather",
        "Look up the weather for a place.",
        json!({
            "type": "object",
            "properties": { "location": { "type": "string" } },
            "required": ["location"]
        }),
    )
}

#[test]
fn a_request_becomes_a_streaming_chat_body() {
    let request = Request::new("gemma:latest")
        .with_system("You are terse.")
        .with_message(Message::user("Is the nacelle warm?"))
        .with_max_tokens(256);
    let body = ollama::request_body(&request);

    assert_eq!(body["model"], json!("gemma:latest"));
    assert_eq!(body["stream"], json!(true), "the whole point is streaming");
    assert_eq!(body["options"]["num_predict"], json!(256));
    assert_eq!(
        body["messages"],
        json!([
            { "role": "system", "content": "You are terse." },
            { "role": "user", "content": "Is the nacelle warm?" },
        ]),
        "the system prompt is an ordinary first message here"
    );
    assert!(body.get("tools").is_none(), "no tools, no key");
    assert!(
        body.get("think").is_none(),
        "models that cannot think reject the field rather than ignoring it"
    );
}

#[test]
fn thinking_is_asked_for_only_when_it_was_asked_for() {
    let request = Request::new("nemotron-3-nano:30b").with_thinking(true);
    assert_eq!(ollama::request_body(&request)["think"], json!(true));
}

#[test]
fn a_tool_declaration_becomes_a_function_declaration() {
    let request = Request::new("m").with_tool(tool());
    let body = ollama::request_body(&request);

    assert_eq!(body["tools"][0]["type"], json!("function"));
    assert_eq!(body["tools"][0]["function"]["name"], json!("get_weather"));
    assert_eq!(
        body["tools"][0]["function"]["parameters"],
        tool().input_schema,
        "the schema travels whole, under the name Ollama gives it"
    );
}

#[test]
fn a_tool_result_goes_back_as_its_own_tool_message() {
    let call = ToolCall {
        id: "get_weather-0".to_string(),
        name: "get_weather".to_string(),
        input: json!({ "location": "Earth" }),
    };
    let request = Request::new("m")
        .with_message(Message::user("Weather?"))
        .with_message(Message::new(
            Role::Assistant,
            vec![
                Content::Text("Looking.".to_string()),
                Content::ToolUse(call.clone()),
            ],
        ))
        .with_message(Message::new(
            Role::User,
            vec![Content::ToolResult {
                id: call.id.clone(),
                output: "18C".to_string(),
                is_error: false,
            }],
        ));

    let body = ollama::request_body(&request);
    let messages = body["messages"].as_array().expect("messages");

    assert_eq!(messages.len(), 3, "the result is a message, not a block");
    assert_eq!(messages[1]["content"], json!("Looking."));
    assert_eq!(
        messages[1]["tool_calls"][0]["function"],
        json!({ "name": "get_weather", "arguments": { "location": "Earth" } })
    );
    assert_eq!(messages[2]["role"], json!("tool"));
    assert_eq!(messages[2]["content"], json!("18C"));
    assert_eq!(
        messages[2]["tool_name"],
        json!("get_weather"),
        "Ollama matches a result to its call by name, so the name has to \
         be recovered from the history the id points into"
    );
}

#[test]
fn a_failed_tool_says_so_in_the_only_place_there_is() {
    let request = Request::new("m").with_message(Message::new(
        Role::User,
        vec![Content::ToolResult {
            id: "unknown".to_string(),
            output: "no such file".to_string(),
            is_error: true,
        }],
    ));
    let body = ollama::request_body(&request);

    assert_eq!(body["messages"][0]["content"], json!("error: no such file"));
    assert!(
        body["messages"][0].get("tool_name").is_none(),
        "an id with no call behind it is left unnamed rather than guessed"
    );
}

#[test]
fn a_thinking_turn_is_sent_back_as_it_arrived() {
    let request = Request::new("m").with_message(Message::new(
        Role::Assistant,
        vec![
            Content::Thinking {
                text: "Weighing it up.".to_string(),
                signature: None,
            },
            Content::Text("Yes.".to_string()),
        ],
    ));
    let body = ollama::request_body(&request);

    assert_eq!(body["messages"][0]["thinking"], json!("Weighing it up."));
    assert_eq!(body["messages"][0]["content"], json!("Yes."));
}

// ------------------------------------------------------- the model list

#[test]
fn the_model_list_is_what_a_picker_needs() {
    let models = ollama::parse_models(TAGS).expect("a tag list");

    assert_eq!(models.len(), 2);
    assert_eq!(
        models[0].name, "gemma4:31b-it-q4_K_M",
        "sorted, because the server's order is not"
    );
    assert_eq!(models[0].size, 18_600_000_000);
    assert_eq!(models[0].parameter_size.as_deref(), Some("31.0B"));
    assert_eq!(models[0].quantization.as_deref(), Some("Q4_K_M"));
    assert_eq!(models[1].name, "gemma:latest");
    assert_eq!(models[1].quantization, None, "absent is not empty");
}

#[test]
fn a_model_list_that_is_not_one_is_a_protocol_error() {
    assert!(matches!(
        ollama::parse_models("not json"),
        Err(BackendError::Protocol(_))
    ));
    assert!(matches!(
        ollama::parse_models(r#"{"something_else": []}"#),
        Err(BackendError::Protocol(_))
    ));
    assert_eq!(
        ollama::parse_models(r#"{"models": []}"#).expect("an empty server"),
        Vec::new(),
        "a server with nothing pulled is not an error"
    );
}

// ------------------------------------------------------------- the host

fn host_from(value: &str) -> String {
    let mut env = HashMap::new();
    env.insert("OLLAMA_HOST".to_string(), value.to_string());
    Ollama::from_env(&env).host().to_string()
}

#[test]
fn the_host_defaults_to_this_machine() {
    let empty: HashMap<String, String> = HashMap::new();

    assert_eq!(Ollama::from_env(&empty).host(), "http://localhost:11434");
    assert_eq!(host_from("   "), "http://localhost:11434");
}

#[test]
fn the_host_accepts_what_ollamas_own_tools_accept() {
    assert_eq!(host_from("box:11434"), "http://box:11434");
    assert_eq!(host_from(":9999"), "http://localhost:9999");
    assert_eq!(host_from("box"), "http://box:11434");
    assert_eq!(host_from("http://box:11434/"), "http://box:11434");
    assert_eq!(host_from("HTTP://box"), "http://box:11434");
    assert_eq!(host_from("[::1]"), "http://[::1]:11434");
    assert_eq!(host_from("[::1]:9999"), "http://[::1]:9999");
    assert_eq!(
        host_from("http://box/ollama/"),
        "http://box:11434/ollama",
        "a reverse proxy keeps its path prefix"
    );
}

#[test]
fn a_password_in_the_host_is_dropped_at_the_door() {
    // This client does not send them, and the host string goes into
    // every error message it produces. A secret one failed request away
    // from a log is not a secret.
    let host = host_from("http://tom:hunter2@box:11434");

    assert_eq!(host, "http://box:11434");
    assert!(!host.contains("hunter2"));
    assert!(!host.contains("tom"));
}

#[test]
fn an_https_host_is_kept_as_one() {
    // A user whose models run on the machine in the next room, behind a
    // TLS proxy, is still running their own models. The scheme they
    // wrote is the scheme that is used.
    assert_eq!(host_from("https://box"), "https://box:11434");
}

#[test]
fn the_backend_is_named_after_the_provider_and_nothing_else() {
    let backend: Box<dyn Backend> = Box::new(Ollama::at("http://box:11434"));

    assert_eq!(backend.name(), "ollama");
    assert!(
        !backend.name().contains("box"),
        "a name identifies the provider, not this machine's route to it"
    );
}

#[test]
fn the_events_are_the_same_ones_the_contract_stub_produces() {
    // The contract test pins two stubs against each other; this pins the
    // real Ollama translation against the same expected stream, so that
    // "the receiver cannot tell who answered" stays true of the backend
    // and not only of the stubs.
    let events = collect(TOOL_TURN).expect("a tool turn");
    let shapes: Vec<&'static str> = events
        .iter()
        .map(|event| match event {
            StreamEvent::Start { .. } => "start",
            StreamEvent::Text(_) => "text",
            StreamEvent::Thinking(_) => "thinking",
            StreamEvent::ToolCall(_) => "tool",
            StreamEvent::End { .. } => "end",
        })
        .collect();

    assert_eq!(shapes, vec!["start", "text", "tool", "end"]);
}

#[test]
fn a_recorded_turn_round_trips_into_history() {
    // What the reader produces has to be something the writer accepts:
    // the tool call that comes out of a stream is the tool call that
    // goes back in as history, id and all.
    let events = collect(TOOL_TURN).expect("a tool turn");
    let call = tool_calls_of(&events)[0].clone();

    let request = Request::new("nemotron-3-nano:30b")
        .with_message(Message::new(
            Role::Assistant,
            vec![Content::ToolUse(call.clone())],
        ))
        .with_message(Message::new(
            Role::User,
            vec![Content::ToolResult {
                id: call.id.clone(),
                output: "18C".to_string(),
                is_error: false,
            }],
        ));

    let body: Value = ollama::request_body(&request);
    assert_eq!(body["messages"][1]["tool_name"], json!("get_weather"));
}
