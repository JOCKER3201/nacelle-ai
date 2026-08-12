//! ADVERSARY PROBE — measurement only, not a fix and not a regression suite.
//!
//! One probe secret, planted in every string position a `Request` has,
//! sealed and encoded through the real backend against a transport that
//! is a `Vec<u8>`. The assertion is on the BYTES the transport was
//! handed, which is the last thing before a socket.

use std::collections::VecDeque;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nacelle_ai::backend::anthropic::{self, Anthropic, HttpResponse, Retry, Transport};
use nacelle_ai::credentials::Credential;
use nacelle_ai::{
    Backend, BackendError, Consent, Content, Flow, Manifest, Message, Policy, Remote, Request,
    Role, Seal, ToolCall, ToolDeclaration, Trigger,
};
use serde_json::{json, Value};

/// A real-shaped Anthropic key: the prefix plus a long base64url body.
const PROBE: &str =
    "sk-ant-api03-Ae7Kq2ZwR4tYuIoP1sDfGhJkLzXcVbNm0987654321QwErTyUiOpAsDfGhJkLzXcVbNm";

const MODEL: &str = anthropic::DEFAULT_MODEL.id;
const CREDENTIAL: &str = "sk-ant-credential-not-the-probe-value-here";

struct Sent {
    body: Vec<u8>,
}

struct Stub {
    replies: Mutex<VecDeque<String>>,
    sent: Mutex<Vec<Sent>>,
}

struct Handle(Arc<Stub>);

impl Transport for Handle {
    fn post(
        &self,
        _url: &str,
        _headers: &[(&'static str, String)],
        body: &[u8],
    ) -> Result<HttpResponse, BackendError> {
        self.0.sent.lock().unwrap().push(Sent {
            body: body.to_vec(),
        });
        let reply = self.0.replies.lock().unwrap().pop_front().unwrap_or_default();
        Ok(HttpResponse {
            status: 200,
            retry_after: None,
            body: Box::new(Once {
                bytes: reply.into_bytes(),
                at: 0,
            }),
        })
    }
}

struct Once {
    bytes: Vec<u8>,
    at: usize,
}

impl Read for Once {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let take = out.len().min(self.bytes.len() - self.at);
        out[..take].copy_from_slice(&self.bytes[self.at..self.at + take]);
        self.at += take;
        Ok(take)
    }
}

fn frame(name: &str, data: Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

fn a_plain_turn() -> String {
    [
        frame(
            "message_start",
            json!({"type":"message_start","message":{"id":"msg_1","type":"message",
                   "role":"assistant","model":MODEL,"content":[],"stop_reason":Value::Null,
                   "usage":{"input_tokens":1,"output_tokens":1}}}),
        ),
        frame(
            "content_block_start",
            json!({"type":"content_block_start","index":0,
                   "content_block":{"type":"text","text":""}}),
        ),
        frame(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,
                   "delta":{"type":"text_delta","text":"ok"}}),
        ),
        frame("content_block_stop", json!({"type":"content_block_stop","index":0})),
        frame(
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},
                   "usage":{"output_tokens":1}}),
        ),
        frame("message_stop", json!({"type":"message_stop"})),
    ]
    .concat()
}

fn agreed() -> Seal {
    Seal::new(
        anthropic::NAME,
        Policy::new(Remote::Ready),
        Trigger::UserAsked,
        |_: &Manifest| Consent::Send,
    )
}

/// Runs the whole road and returns (was the turn sent, the raw bytes).
fn wire(request: &Request) -> (Result<(), BackendError>, String) {
    let stub = Arc::new(Stub {
        replies: Mutex::new(VecDeque::from(vec![a_plain_turn()])),
        sent: Mutex::new(Vec::new()),
    });
    let mut backend = Anthropic::with_transport(
        Credential::api_key(CREDENTIAL),
        agreed(),
        Handle(Arc::clone(&stub)),
    )
    .with_retry(Retry {
        attempts: 1,
        backoff: Duration::ZERO,
        cap: Duration::ZERO,
    });

    let result = backend.send(request, &mut |_| Flow::Continue);
    let bytes = stub
        .sent
        .lock()
        .unwrap()
        .iter()
        .map(|sent| String::from_utf8_lossy(&sent.body).into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    (result, bytes)
}

fn report(place: &str, request: &Request) -> bool {
    let (result, bytes) = wire(request);
    let leaked = bytes.contains(PROBE);
    println!(
        "[{}] {place}: sent={} bytes_len={} outcome={:?}",
        if leaked { "LEAK" } else { "held" },
        !bytes.is_empty(),
        bytes.len(),
        result.as_ref().err().map(|e| e.to_string())
    );
    leaked
}

fn base() -> Request {
    Request::new(MODEL).with_message(Message::user("what is the weather?"))
}

fn a_tool() -> ToolDeclaration {
    ToolDeclaration::new(
        "get_weather",
        "Look up the weather.",
        json!({"type":"object","properties":{"location":{"type":"string"}},
               "required":["location"],"additionalProperties":false}),
    )
}

#[test]
fn every_string_position_in_a_request() {
    let mut leaks: Vec<&str> = Vec::new();

    // ---- control: ordinary prose. Must be held.
    if report(
        "CONTROL Message::user text",
        &Request::new(MODEL).with_message(Message::user(format!("my key is {PROBE} ok"))),
    ) {
        leaks.push("CONTROL message text");
    }

    // ---- Request::model
    if report("Request::model", &Request::new(PROBE).with_message(Message::user("hi"))) {
        leaks.push("Request::model");
    }

    // ---- Request::system
    if report(
        "Request::system",
        &base().with_system(format!("context: {PROBE}")),
    ) {
        leaks.push("Request::system");
    }

    // ---- Content::Thinking { text }
    if report(
        "Content::Thinking.text (signed, so it is encoded)",
        &base().with_message(Message::new(
            Role::Assistant,
            vec![Content::Thinking {
                text: format!("I saw {PROBE}"),
                signature: Some("sig-1".to_string()),
            }],
        )),
    ) {
        leaks.push("Thinking.text");
    }

    // ---- Content::Thinking { signature }
    if report(
        "Content::Thinking.signature",
        &base().with_message(Message::new(
            Role::Assistant,
            vec![Content::Thinking {
                text: "clean reasoning".to_string(),
                signature: Some(PROBE.to_string()),
            }],
        )),
    ) {
        leaks.push("Thinking.signature");
    }

    // ---- ToolCall::id (declared tool)
    if report(
        "ToolCall::id",
        &base().with_tool(a_tool()).with_message(Message::new(
            Role::Assistant,
            vec![Content::ToolUse(ToolCall {
                id: PROBE.to_string(),
                name: "get_weather".to_string(),
                input: json!({"location": "Earth"}),
            })],
        )),
    ) {
        leaks.push("ToolCall::id");
    }

    // ---- ToolCall::name, undeclared
    if report(
        "ToolCall::name (undeclared)",
        &base().with_message(Message::new(
            Role::Assistant,
            vec![Content::ToolUse(ToolCall {
                id: "toolu_1".to_string(),
                name: PROBE.to_string(),
                input: json!({}),
            })],
        )),
    ) {
        leaks.push("ToolCall::name undeclared");
    }

    // ---- ToolCall::input value
    if report(
        "ToolCall::input value",
        &base().with_tool(a_tool()).with_message(Message::new(
            Role::Assistant,
            vec![Content::ToolUse(ToolCall {
                id: "toolu_1".to_string(),
                name: "get_weather".to_string(),
                input: json!({"location": PROBE}),
            })],
        )),
    ) {
        leaks.push("ToolCall::input value");
    }

    // ---- ToolCall::input key, undeclared
    if report(
        "ToolCall::input key (undeclared)",
        &base().with_tool(a_tool()).with_message(Message::new(
            Role::Assistant,
            vec![Content::ToolUse(ToolCall {
                id: "toolu_1".to_string(),
                name: "get_weather".to_string(),
                input: json!({PROBE: "x"}),
            })],
        )),
    ) {
        leaks.push("ToolCall::input key");
    }

    // ---- ToolResult::id
    if report(
        "ToolResult::id",
        &base().with_message(Message::new(
            Role::User,
            vec![Content::ToolResult {
                id: PROBE.to_string(),
                output: "sunny".to_string(),
                is_error: false,
            }],
        )),
    ) {
        leaks.push("ToolResult::id");
    }

    // ---- ToolResult::output
    if report(
        "ToolResult::output",
        &base().with_message(Message::new(
            Role::User,
            vec![Content::ToolResult {
                id: "toolu_1".to_string(),
                output: PROBE.to_string(),
                is_error: false,
            }],
        )),
    ) {
        leaks.push("ToolResult::output");
    }

    // ---- ToolDeclaration::name
    if report(
        "ToolDeclaration::name",
        &base().with_tool(ToolDeclaration::new(
            PROBE,
            "Look up the weather.",
            json!({"type":"object","properties":{}}),
        )),
    ) {
        leaks.push("ToolDeclaration::name");
    }

    // ---- ToolDeclaration::description
    if report(
        "ToolDeclaration::description",
        &base().with_tool(ToolDeclaration::new(
            "get_weather",
            format!("Look up the weather. {PROBE}"),
            json!({"type":"object","properties":{}}),
        )),
    ) {
        leaks.push("ToolDeclaration::description");
    }

    // ---- input_schema VALUE
    if report(
        "ToolDeclaration::input_schema value",
        &base().with_tool(ToolDeclaration::new(
            "get_weather",
            "Look up the weather.",
            json!({"type":"object","properties":{"location":{"description": PROBE}}}),
        )),
    ) {
        leaks.push("input_schema value");
    }

    // ---- input_schema KEY (a property name)
    if report(
        "ToolDeclaration::input_schema key",
        &base().with_tool(ToolDeclaration::new(
            "get_weather",
            "Look up the weather.",
            json!({"type":"object","properties":{PROBE:{"type":"string"}}}),
        )),
    ) {
        leaks.push("input_schema key");
    }

    println!("\n==== LEAKED POSITIONS: {leaks:?}");
    assert!(leaks.is_empty(), "probe reached the transport from: {leaks:?}");
}

/// A secret with no named provider shape — only high entropy. The id
/// rule deliberately refuses to consult the entropy rule.
const OPAQUE: &str = "u8jzPde0IgxLd6GncfBAepfJBd0Kh8oOOL8dKLzdocJ2";

#[test]
fn an_opaque_secret_in_an_identifier() {
    let request = base()
        .with_tool(a_tool())
        .with_message(Message::new(
            Role::Assistant,
            vec![Content::ToolUse(ToolCall {
                id: OPAQUE.to_string(),
                name: "get_weather".to_string(),
                input: json!({"location": "Earth"}),
            })],
        ))
        .with_message(Message::new(
            Role::User,
            vec![Content::ToolResult {
                id: OPAQUE.to_string(),
                output: "sunny".to_string(),
                is_error: false,
            }],
        ));

    // Control: the same string in prose IS cut, so the shape is one
    // layer 2 does recognise.
    let (_, prose) = wire(&base().with_message(Message::user(format!("value {OPAQUE} end"))));
    println!("in prose: leaked={}", prose.contains(OPAQUE));

    let (result, bytes) = wire(&request);
    println!(
        "in tool ids: leaked={} outcome={:?}",
        bytes.contains(OPAQUE),
        result.as_ref().err().map(|e| e.to_string())
    );
    assert!(
        !bytes.contains(OPAQUE),
        "an opaque secret reached the transport through a tool call id"
    );
}

/// The manifest promises: "Nothing reaches a socket that this number did
/// not include." Measured against the bytes.
#[test]
fn the_manifest_size_covers_what_reaches_the_socket() {
    use std::sync::Mutex as M;

    let seen: Arc<M<Vec<Manifest>>> = Arc::new(M::new(Vec::new()));
    let record = Arc::clone(&seen);

    let stub = Arc::new(Stub {
        replies: Mutex::new(VecDeque::from(vec![a_plain_turn()])),
        sent: Mutex::new(Vec::new()),
    });

    let seal = Seal::new(
        anthropic::NAME,
        Policy::new(Remote::Ready),
        Trigger::UserAsked,
        move |manifest: &Manifest| {
            record.lock().unwrap().push(manifest.clone());
            Consent::Send
        },
    );

    let mut backend = Anthropic::with_transport(
        Credential::api_key(CREDENTIAL),
        seal,
        Handle(Arc::clone(&stub)),
    )
    .with_retry(Retry {
        attempts: 1,
        backoff: Duration::ZERO,
        cap: Duration::ZERO,
    });

    // A signed thinking block whose signature is the probe, plus a
    // short prose turn. Nothing else.
    let request = Request::new(MODEL)
        .with_message(Message::user("hi"))
        .with_message(Message::new(
            Role::Assistant,
            vec![Content::Thinking {
                text: "ok".to_string(),
                signature: Some(PROBE.to_string()),
            }],
        ));

    let _ = backend.send(&request, &mut |_| Flow::Continue);

    let manifests = seen.lock().unwrap();
    let payload = nacelle_ai::payload_of(&request);
    let sent = stub.sent.lock().unwrap();
    let bytes = sent
        .first()
        .map(|first| String::from_utf8_lossy(&first.body).into_owned())
        .unwrap_or_default();

    println!("payload_of     = {} -> {payload:?}", payload.len());
    println!("probe on wire  = {}", bytes.contains(PROBE));
    println!("probe in payload_of = {}", payload.contains(PROBE));
    match manifests.first() {
        Some(manifest) => {
            println!("manifest.bytes = {}", manifest.bytes);
            println!("---- manifest as the user reads it ----\n{}", manifest.render());
        }
        // The turn was refused before layer 4: this probe's signature is
        // a `sk-ant-` key, and a signature is now read for named shapes
        // and the turn stopped on a hit. Nothing was shown because
        // nothing was going to be sent.
        None => println!("no manifest: the turn did not get that far"),
    }

    assert!(
        !bytes.contains(PROBE) || payload.contains(PROBE),
        "a string reached the socket that the manifest's size did not include"
    );
}

#[test]
fn print_the_actual_bytes_for_the_signature_case() {
    let request = base().with_message(Message::new(
        Role::Assistant,
        vec![Content::Thinking {
            text: "clean reasoning".to_string(),
            signature: Some(PROBE.to_string()),
        }],
    ));
    let (_, bytes) = wire(&request);
    println!("BODY ON THE WIRE:\n{bytes}");

    let schema = base().with_tool(ToolDeclaration::new(
        "get_weather",
        "Look up the weather.",
        json!({"type":"object","properties":{PROBE:{"type":"string"}}}),
    ));
    let (_, bytes) = wire(&schema);
    println!("\nSCHEMA-KEY BODY ON THE WIRE:\n{bytes}");

    let ids = base()
        .with_tool(a_tool())
        .with_message(Message::new(
            Role::Assistant,
            vec![Content::ToolUse(ToolCall {
                id: OPAQUE.to_string(),
                name: "get_weather".to_string(),
                input: json!({"location": "Earth"}),
            })],
        ));
    let (_, bytes) = wire(&ids);
    println!("\nOPAQUE-ID BODY ON THE WIRE:\n{bytes}");
}

/// Reachability check for the id carve-out: Ollama mints
/// `id = format!("{name}-{index}")`, so the id an undeclared call
/// carries is the name the model wrote. Does the NAME check catch what
/// the ID check is not allowed to see?
#[test]
fn an_opaque_name_that_ollama_would_turn_into_an_id() {
    let ollama_shaped = base().with_message(Message::new(
        Role::Assistant,
        vec![Content::ToolUse(ToolCall {
            id: format!("{OPAQUE}-0"),
            name: OPAQUE.to_string(),
            input: json!({}),
        })],
    ));
    let (result, bytes) = wire(&ollama_shaped);
    println!(
        "undeclared opaque name+id: on_wire={} outcome={:?}",
        bytes.contains(OPAQUE),
        result.as_ref().err().map(|e| e.to_string())
    );

    // And the same, with the name declared so the name check is skipped.
    let declared = base()
        .with_tool(ToolDeclaration::new(
            OPAQUE,
            "d",
            json!({"type": "object", "properties": {}}),
        ))
        .with_message(Message::new(
            Role::Assistant,
            vec![Content::ToolUse(ToolCall {
                id: format!("{OPAQUE}-0"),
                name: OPAQUE.to_string(),
                input: json!({}),
            })],
        ));
    let (result, bytes) = wire(&declared);
    println!(
        "declared opaque name+id:   on_wire={} outcome={:?}",
        bytes.contains(OPAQUE),
        result.as_ref().err().map(|e| e.to_string())
    );
}
