//! The layers, on the path they are supposed to be on.
//!
//! `redact.rs` and `supervise.rs` test the layers and the road as
//! pieces: given a payload, this comes out; given a policy, this is
//! decided. Every one of them passed on the day the layers were reached
//! by nothing at all, which is the failure this file exists to make
//! impossible to have again. So nothing here calls a layer. Everything
//! here calls [`Backend::send`] on the real Anthropic backend, over a
//! transport that is a `Vec<u8>`, and then looks at **the bytes that
//! reached the socket** — because that is the only question worth
//! asking of a guard: not whether it works, but whether anything can get
//! past it.
//!
//! The strongest property is not tested here, because it cannot be: a
//! [`Sealed`](nacelle_ai::Sealed) request is the only thing the encoder
//! accepts and [`Seal::seal`] is its only constructor, so a call site
//! that skips the layers does not fail a test — it fails to compile.
//! What is tested is everything that a compiler cannot say: that the
//! layers ran, in order, on every part of the request, and that a "no"
//! anywhere stops the bytes before a socket is opened.
//!
//! Nothing here touches a network.

use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nacelle_ai::backend::anthropic::{self, Anthropic, HttpResponse, Retry, Transport};
use nacelle_ai::backend::ollama;
use nacelle_ai::credentials::Credential;
use nacelle_ai::redact::scan::{scan, Kind};
use nacelle_ai::{
    Attempts, Backend, BackendError, Consent, Content, Flow, Grounds, Manifest, Message, Policy,
    Remote, Removal, Request, Review, Reviewer, Role, Seal, ToolCall, ToolDeclaration, Trigger,
};
use serde_json::json;

/// A shape layer 2 knows, in a string a test can look for afterwards.
const KEY: &str = "sk-ant-api03-Zx8Qv2Lm4Np7Rt1Ws9Yb3Cd6Ef0Gh5Ij8Kl2Mn4Op6Qr8St0Uv2Wx4Yz";

/// The model id used everywhere below.
const MODEL: &str = anthropic::DEFAULT_MODEL.id;

// ---------------------------------------------------------------- stubs

/// A transport that answers with the same canned turn every time, or
/// with a failure, and remembers every request it was given.
struct Wire {
    answer: Mutex<Result<String, BackendError>>,
    sent: Mutex<Vec<Vec<u8>>>,
}

impl Wire {
    fn ok() -> Arc<Self> {
        Arc::new(Wire {
            answer: Mutex::new(Ok(a_turn())),
            sent: Mutex::new(Vec::new()),
        })
    }

    fn failing(err: BackendError) -> Arc<Self> {
        Arc::new(Wire {
            answer: Mutex::new(Err(err)),
            sent: Mutex::new(Vec::new()),
        })
    }

    /// How many times a socket would have been opened.
    fn requests(&self) -> usize {
        self.sent.lock().unwrap().len()
    }

    /// Every byte that left, as one string. Not parsed as JSON on
    /// purpose: a secret that survived inside an escape sequence or a
    /// field nobody thought to look at is still a secret that left.
    fn bytes(&self) -> String {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .map(|body| String::from_utf8_lossy(body).into_owned())
            .collect()
    }
}

struct Held(Arc<Wire>);

impl Transport for Held {
    fn post(
        &self,
        _url: &str,
        _headers: &[(&'static str, String)],
        body: &[u8],
    ) -> Result<HttpResponse, BackendError> {
        self.0.sent.lock().unwrap().push(body.to_vec());
        match &*self.0.answer.lock().unwrap() {
            Ok(turn) => Ok(HttpResponse {
                status: 200,
                retry_after: None,
                body: Box::new(Bytes(turn.clone().into_bytes(), 0)),
            }),
            Err(err) => Err(err.clone()),
        }
    }
}

struct Bytes(Vec<u8>, usize);

impl Read for Bytes {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let take = out.len().min(self.0.len() - self.1);
        out[..take].copy_from_slice(&self.0[self.1..self.1 + take]);
        self.1 += take;
        Ok(take)
    }
}

fn a_turn() -> String {
    [
        format!(
            "event: message_start\ndata: {}\n\n",
            json!({"message": {"model": MODEL, "usage": {"input_tokens": 3, "output_tokens": 1}}})
        ),
        format!(
            "event: content_block_delta\ndata: {}\n\n",
            json!({"index": 0, "delta": {"type": "text_delta", "text": "hello"}})
        ),
        format!(
            "event: message_delta\ndata: {}\n\n",
            json!({"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}})
        ),
        "event: message_stop\ndata: {}\n\n".to_string(),
    ]
    .concat()
}

/// Every manifest this session was shown.
#[derive(Clone, Default)]
struct Shown(Arc<Mutex<Vec<Manifest>>>);

impl Shown {
    fn count(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    fn last(&self) -> Manifest {
        self.0.lock().unwrap().last().cloned().expect("a manifest")
    }

    /// A user who reads every manifest and agrees.
    fn agreeing(&self) -> impl FnMut(&Manifest) -> Consent + Send {
        let seen = Arc::clone(&self.0);
        move |manifest: &Manifest| {
            seen.lock().unwrap().push(manifest.clone());
            Consent::Send
        }
    }

    /// A user who reads it and does not.
    fn refusing(&self) -> impl FnMut(&Manifest) -> Consent + Send {
        let seen = Arc::clone(&self.0);
        move |manifest: &Manifest| {
            seen.lock().unwrap().push(manifest.clone());
            Consent::refuse("not this")
        }
    }
}

/// A layer 3 that asks for one quote to go, so the test can prove the
/// removal reached the wire rather than only the manifest.
struct Fussy {
    quote: String,
    asked: Arc<Mutex<Vec<String>>>,
}

impl Reviewer for Fussy {
    fn review(&mut self, payload: &str) -> Review {
        self.asked.lock().unwrap().push(payload.to_string());
        Review {
            removals: vec![Removal::new(&self.quote, "a private matter")],
            note: None,
        }
    }
}

// -------------------------------------------------------------- running

fn wired(seal: Seal, wire: &Arc<Wire>) -> Anthropic<Held> {
    Anthropic::with_transport(
        Credential::api_key("sk-ant-test-credential-value"),
        seal,
        Held(Arc::clone(wire)),
    )
    // The failure tests would otherwise sit here sleeping through a
    // backoff schedule that is tested elsewhere.
    .with_retry(Retry {
        attempts: 1,
        backoff: Duration::ZERO,
        cap: Duration::ZERO,
    })
}

fn seal_with(
    trigger: Trigger,
    discloser: impl FnMut(&Manifest) -> Consent + Send + 'static,
) -> Seal {
    Seal::new(anthropic::NAME, Policy::new(Remote::Ready), trigger, discloser)
}

fn send(backend: &mut Anthropic<Held>, request: &Request) -> Result<(), BackendError> {
    backend.send(request, &mut |_| Flow::Continue)
}

fn ask(text: &str) -> Request {
    Request::new(MODEL).with_message(Message::user(text))
}

/// The trigger a counter produces once the same work has failed twice.
fn twice_failed(task: &str) -> Trigger {
    let mut attempts = Attempts::new();
    assert!(
        attempts.failed(task).is_none(),
        "one failure is a mistake, not a pattern"
    );
    attempts.failed(task).expect("two failures are a pattern")
}

// ---------------------------------------------------- layer 2, on the wire

/// The one that would have been true of the old code and was not: a key
/// in the conversation reaches the socket.
#[test]
fn a_key_in_the_conversation_does_not_reach_the_socket() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);

    send(
        &mut backend,
        &ask(&format!("my key is {KEY}, what is wrong with it?")),
    )
    .expect("the turn ran");

    let sent = wire.bytes();
    assert_eq!(wire.requests(), 1);
    assert!(!sent.contains(KEY), "the key left the machine");
    assert!(
        sent.contains("an Anthropic API key"),
        "the marker that says something was taken out is missing: {sent}"
    );
}

/// Layer 2 is on every string a request carries, not only on the pretty
/// one. A tool result is where a file's contents actually live.
#[test]
fn a_key_in_a_tool_result_or_a_tool_argument_does_not_reach_the_socket() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);

    let request = Request::new(MODEL)
        .with_system(format!("the machine's own note: {KEY}"))
        .with_message(Message::user("look at my config"))
        .with_message(Message::new(
            Role::Assistant,
            vec![Content::ToolUse(ToolCall {
                id: "call_1".to_string(),
                name: "read_config".to_string(),
                input: json!({"path": "/home/u/.config/x", "note": KEY}),
            })],
        ))
        .with_message(Message::new(
            Role::User,
            vec![Content::ToolResult {
                id: "call_1".to_string(),
                output: format!("token = {KEY}"),
                is_error: false,
            }],
        ))
        .with_tool(ToolDeclaration::new(
            "read_config",
            format!("Read a file. Example: {KEY}"),
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        ));

    send(&mut backend, &request).expect("the turn ran");

    let sent = wire.bytes();
    assert!(
        !sent.contains(KEY),
        "a key survived somewhere in the request: {sent}"
    );
    // The identifiers are rewritten rather than kept, and a result still
    // names the call it answers — which is the only thing the endpoint
    // asks of them.
    let (call, result) = the_two_ids(&wire);
    assert_eq!(call, result);
    assert!(!call.is_empty());
    assert!(sent.contains("read_config"));
    // The argument NAME survives too — a marker in place of a key is a
    // tool call that cannot be run.
    assert!(sent.contains("path"));
}

/// A marker with nothing to explain it reads as noise, and a model that
/// reads it as noise answers as though nothing was missing.
#[test]
fn the_far_model_is_told_that_something_was_withheld() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);

    send(&mut backend, &ask(&format!("here: {KEY}"))).expect("the turn ran");
    assert!(wire.bytes().contains("ask the user for it"));

    // And not otherwise: a note on every turn is a note nothing reads.
    let clean = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &clean);
    send(&mut backend, &ask("nothing to see here")).expect("the turn ran");
    assert!(!clean.bytes().contains("ask the user for it"));
}

// ---------------------------------------------------- layer 3, on the wire

#[test]
fn what_the_local_model_asks_to_remove_is_removed_from_what_is_sent() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let mut backend = wired(
        seal_with(Trigger::UserAsked, shown.agreeing()).with_reviewer(Fussy {
            quote: "the diagnosis was pneumonia".to_string(),
            asked: Arc::clone(&asked),
        }),
        &wire,
    );

    send(
        &mut backend,
        &ask("in the note it says the diagnosis was pneumonia, is that consistent?"),
    )
    .expect("the turn ran");

    let sent = wire.bytes();
    assert!(!sent.contains("pneumonia"), "layer 3 removed nothing: {sent}");
    assert!(sent.contains("a private matter"));
    // It read the payload after layer 2, which is the order that makes
    // it the last layer rather than the first.
    let payload = asked.lock().unwrap().first().cloned().expect("a review");
    assert!(payload.contains("pneumonia"));
}

/// An absent layer 3 and a layer 3 that found nothing are not the same
/// assurance, and the manifest has to say which one happened.
#[test]
fn a_manifest_says_so_when_no_local_model_reviewed_the_payload() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);

    send(&mut backend, &ask("hello")).expect("the turn ran");

    let manifest = shown.last();
    assert!(
        manifest
            .notes
            .iter()
            .any(|note| note.contains("no local model reviewed")),
        "the manifest did not admit that layer 3 did not run: {manifest}"
    );
}

// ---------------------------------------------------- layer 4, on the wire

/// The whole point of layer 4: the bytes wait for the user, and "no"
/// means no socket was opened at all.
#[test]
fn nothing_is_sent_until_the_user_has_seen_the_manifest_and_agreed() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.refusing()), &wire);

    let err = send(&mut backend, &ask("what is the weather?")).expect_err("refused");

    assert_eq!(shown.count(), 1, "the user was not shown anything");
    assert_eq!(wire.requests(), 0, "bytes left despite the refusal");
    // Never retried: asking again would put the same manifest in front
    // of the same person, and asking twice is how a refusal gets clicked
    // through.
    assert!(!err.is_retryable());
    match err {
        BackendError::Withheld(tell) => {
            assert!(tell.contains("nothing left this machine"));
            assert!(tell.contains("not this"), "the user's reason was dropped");
        }
        other => panic!("a refusal should not look like {other:?}"),
    }
}

/// Shown when there is something new to see, and not on every turn — a
/// manifest before every request is one that gets clicked through, which
/// is worse than none because it trains the reflex it exists to
/// interrupt.
#[test]
fn the_manifest_is_shown_once_a_session_rather_than_once_a_turn() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);

    for turn in 0..3 {
        send(&mut backend, &ask(&format!("question {turn}"))).expect("the turn ran");
    }

    assert_eq!(shown.count(), 1);
    assert_eq!(wire.requests(), 3);
}

/// It never prints what it is describing: a manifest that quoted the
/// payload would be one more copy of the payload, on a screen that may
/// be shared.
#[test]
fn the_manifest_never_repeats_what_it_took_out() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);

    send(&mut backend, &ask(&format!("my key is {KEY}"))).expect("the turn ran");

    let manifest = shown.last();
    let text = manifest.render();
    assert!(!text.contains(KEY));
    assert!(text.contains("an Anthropic API key"));
    assert!(text.contains(anthropic::NAME), "the user is not told where it goes");
}

// ----------------------------------------------------------- the triggers

/// Every trigger in `docs/supervisor.md`, one at a time, each carried
/// through the seal to the sentence the user actually reads.
#[test]
fn every_escalation_trigger_reaches_the_manifest_as_its_own_reason() {
    let cases: Vec<(Trigger, &str)> = vec![
        (Trigger::UserAsked, "you asked"),
        (twice_failed("rename the panel"), "failed"),
        (
            Trigger::context_exceeded(300_000, 8_192).expect("300k does not fit in 8k"),
            "context",
        ),
        (
            Trigger::MissingCapability {
                needed: "tool support in the loaded model".to_string(),
            },
            "no tool support",
        ),
        (
            Trigger::model_asked("this needs reasoning I cannot do").expect("a reason was given"),
            "asked to escalate",
        ),
    ];

    for (trigger, expected) in cases {
        let wire = Wire::ok();
        let shown = Shown::default();
        let mut backend = wired(seal_with(trigger.clone(), shown.agreeing()), &wire);

        send(&mut backend, &ask("go on then")).expect("the turn ran");

        let manifest = shown.last();
        assert!(
            manifest.reason.contains(expected),
            "{trigger:?} reached the user as \"{}\"",
            manifest.reason
        );
    }
}

/// The counter, not the model's opinion: one failure is a mistake and
/// two are a pattern.
#[test]
fn the_repeated_failure_trigger_takes_two_failures_and_not_one() {
    let mut attempts = Attempts::new();
    assert!(attempts.failed("rename the panel").is_none());
    assert!(attempts.failed("rename the panel").is_some());

    // And a success in between forgets the first one.
    let mut attempts = Attempts::new();
    assert!(attempts.failed("rename the panel").is_none());
    attempts.succeeded("rename the panel");
    assert!(attempts.failed("rename the panel").is_none());
}

/// The model may ask, but only with a reason the user can weigh. "I
/// think we should escalate" is not one.
#[test]
fn the_model_cannot_ask_to_escalate_without_saying_why() {
    assert!(Trigger::model_asked("   ").is_none());
    assert!(Trigger::model_asked("the file is larger than I can hold").is_some());
}

// -------------------------------------------------------------- the pin

/// A pin is not advice. It stops the bytes at the seal, before a socket
/// exists, whatever the rest of the machine has on it.
#[test]
fn a_pinned_session_never_opens_a_socket() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);
    backend.seal().pin();

    let err = send(&mut backend, &ask("ask Claude about this")).expect_err("pinned");

    assert_eq!(wire.requests(), 0);
    assert_eq!(shown.count(), 0, "a pinned session should not even ask");
    let BackendError::Withheld(tell) = err else {
        panic!("a pin should read as a refusal to send, not as a failure");
    };
    assert!(tell.contains("pinned to the local model"));
    assert!(tell.contains("Unpin"), "the user is not told how to lift it");
}

/// The user's explicit request and the pin can contradict each other.
/// The pin wins, and says how to lift it — a request that silently
/// overrode it would make the pin a suggestion.
#[test]
fn an_explicit_request_does_not_walk_around_the_pin_at_the_wire() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);
    backend.seal().pin();
    assert!(send(&mut backend, &ask("ask Claude")).is_err());

    // And taking it back is one call, after which the same question
    // goes through.
    backend.seal().unpin();
    send(&mut backend, &ask("ask Claude")).expect("unpinned");
    assert_eq!(wire.requests(), 1);
}

/// The property the whole supervisor is built around: a machine with no
/// network and a machine the user pinned are one code path with two
/// explanations, not two failure modes found at two different times.
#[test]
fn no_network_degrades_exactly_like_a_pin() {
    let wire = Wire::failing(BackendError::Network("no route to host".into()));
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);

    // The first turn finds out the hard way, at a socket.
    let first = send(&mut backend, &ask("first")).expect_err("no network");
    assert!(matches!(first, BackendError::Network(_)));
    assert_eq!(wire.requests(), 1);

    // Every one after it is refused where a pin is refused, and says
    // the same kind of thing.
    let second = send(&mut backend, &ask("second")).expect_err("still no network");
    assert_eq!(wire.requests(), 1, "it reached for the network again");
    let BackendError::Withheld(tell) = second else {
        panic!("an unreachable provider should degrade to a refusal to send");
    };
    assert!(tell.contains("cannot be reached from here"));
    assert_eq!(backend.seal().policy().blocked(), Some(Grounds::Unreachable));

    // Coming back is an act, not a timeout: a supervisor that re-armed
    // itself would go back to reaching for a network that is still not
    // there, one stalled turn at a time.
    assert!(!backend.seal().policy().status().contains("available"));
}

/// A credential the provider rejected is not the same thing as no
/// credential, but it is the same thing to do about it.
#[test]
fn a_rejected_credential_degrades_the_same_way() {
    let wire = Wire::failing(BackendError::Auth("bad key".into()));
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);

    assert!(send(&mut backend, &ask("first")).is_err());
    let second = send(&mut backend, &ask("second")).expect_err("no usable credential");

    assert_eq!(wire.requests(), 1);
    let BackendError::Withheld(tell) = second else {
        panic!("a rejected credential should stop the next turn at the seal");
    };
    assert!(tell.contains("no credential for it on this machine"));
}

/// A bad turn is not a broken network. Rate limits, refusals and
/// unreadable replies are answers, and an answer means the provider is
/// plainly there.
#[test]
fn one_bad_turn_does_not_pin_the_session() {
    let wire = Wire::failing(BackendError::Server {
        status: 500,
        message: "internal".into(),
    });
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);

    assert!(send(&mut backend, &ask("first")).is_err());
    assert!(send(&mut backend, &ask("second")).is_err());
    assert_eq!(wire.requests(), 2, "the second turn was not even tried");
    assert_eq!(backend.seal().policy().blocked(), None);
}

// --------------------------------------------- the other half, deliberately

/// The local backend does not redact, and this is the test that says so
/// out loud rather than leaving it as something nobody wrote down. The
/// layers exist because bytes are about to reach a third party under a
/// credential; a model on this machine is neither, and hiding the user's
/// own key from the agent they asked to look at it would buy nothing.
#[test]
fn the_local_backend_sends_what_it_was_given() {
    let request = ask(&format!("my key is {KEY}, what is wrong with it?"));
    let body = ollama::request_body(&request).to_string();

    assert!(body.contains(KEY));
    assert!(!body.contains("[[redacted"));
}

// ------------------------------------------- the manifest, to the byte

/// Every string of the user's text in an encoded body, in the order the
/// seal walks a request: the system prompt, then each message's blocks,
/// then the tool declarations.
///
/// Written out here rather than borrowed from the crate, and that is the
/// whole value of it. A manifest checked against the crate's own idea of
/// its size would be two functions agreeing with each other, which is
/// exactly what a figure kept alongside the payload does right up until
/// the day it stops.
fn text_on_the_wire(body: &serde_json::Value) -> String {
    fn strings_in(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::String(text) => out.push(text.clone()),
            serde_json::Value::Array(items) => {
                for item in items {
                    strings_in(item, out);
                }
            }
            // Values, never keys — a key is an argument's name and the
            // far side dispatches on it, which is why the seal leaves
            // them alone too.
            serde_json::Value::Object(map) => {
                for entry in map.values() {
                    strings_in(entry, out);
                }
            }
            _ => {}
        }
    }

    let mut pieces: Vec<String> = Vec::new();

    // The model id and a thinking block's signature are on this list
    // because they are on the wire. Neither can be edited — a marker in
    // a model id is a 404 and a marker in a signature is a rejected
    // block — but the manifest's promise is about what reaches a socket,
    // not about what could have been cut. With the signature off this
    // list the manifest read "12 bytes" over a 4252-byte body.
    pieces.push(body["model"].as_str().expect("a model").to_string());

    for block in body["system"].as_array().expect("a system prompt") {
        pieces.push(block["text"].as_str().expect("system text").to_string());
    }

    for message in body["messages"].as_array().expect("messages") {
        for block in message["content"].as_array().expect("content") {
            match block["type"].as_str().expect("a block type") {
                "text" => pieces.push(block["text"].as_str().expect("text").to_string()),
                "thinking" => {
                    pieces.push(block["thinking"].as_str().expect("thinking").to_string());
                    pieces.push(block["signature"].as_str().expect("a signature").to_string());
                }
                "tool_use" => strings_in(&block["input"], &mut pieces),
                "tool_result" => {
                    pieces.push(block["content"].as_str().expect("a result").to_string())
                }
                other => panic!("a block this test does not know how to weigh: {other}"),
            }
        }
    }

    for tool in body["tools"].as_array().expect("tools") {
        pieces.push(tool["description"].as_str().expect("a description").to_string());
        strings_in(&tool["input_schema"], &mut pieces);
    }

    pieces.join("\n\n")
}

/// A request with something of every kind in it, and a secret so that
/// the withheld note is in play too.
fn a_full_request() -> Request {
    Request::new(MODEL)
        .with_system("You run the desktop this agent is part of.")
        .with_message(Message::user(format!("my key is {KEY}, look at the config")))
        .with_message(Message::new(
            Role::Assistant,
            vec![Content::ToolUse(ToolCall {
                id: "call_1".to_string(),
                name: "read_config".to_string(),
                input: json!({"path": "/home/michael/.config/nacelle-desktop/x.conf"}),
            })],
        ))
        .with_message(Message::new(
            Role::User,
            vec![Content::ToolResult {
                id: "call_1".to_string(),
                output: "Theme = crimson\nPanels = 0\n".to_string(),
                is_error: false,
            }],
        ))
        .with_tool(ToolDeclaration::new(
            "read_config",
            "Read a configuration file and report what is in it.",
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        ))
}

/// The number on the manifest is the number of bytes of the user's text
/// that reached the socket. Not approximately, and not a figure that was
/// true earlier in the function that built it: the same walk over the
/// same finished request, which is why there is nothing here that can
/// drift.
#[test]
fn the_manifest_agrees_to_the_byte_with_the_text_that_reached_the_socket() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);

    send(&mut backend, &a_full_request()).expect("the turn ran");

    let sent = wire.sent.lock().unwrap()[0].clone();
    let body: serde_json::Value = serde_json::from_slice(&sent).expect("the body is JSON");
    let manifest = shown.last();

    let went = text_on_the_wire(&body);
    assert!(went.len() > 200, "this test is measuring nothing: {went}");
    assert_eq!(
        manifest.bytes,
        went.len(),
        "the user was told {} bytes and {} went",
        manifest.bytes,
        went.len()
    );
    // Including the note that says something was withheld: it is part of
    // what leaves, so it is part of what the user was told about.
    assert!(went.contains("ask the user for it"), "{went}");
    assert!(!went.contains(KEY));
}

/// The same number, from the other side: what the seal itself says the
/// request weighs. `Sealed::bytes` reads it off the request that is
/// about to be encoded rather than remembering what the manifest said,
/// so the two agreeing is a property of there being one walk rather than
/// of two counts having been kept in step.
#[test]
fn the_seal_and_the_manifest_are_the_same_number_because_they_are_one_walk() {
    let shown = Shown::default();
    let mut seal = seal_with(Trigger::UserAsked, shown.agreeing());

    let sealed = seal.seal(&a_full_request()).expect("the user agreed");
    let manifest = shown.last();

    assert_eq!(manifest.bytes, sealed.bytes());
    assert_eq!(
        sealed.bytes(),
        nacelle_ai::payload_of(sealed.request()).len()
    );
    assert_eq!(
        manifest.bytes,
        sealed.manifest().expect("the first escalation is shown").bytes
    );
}

/// A manifest names the files it can name, and says what is not on the
/// list.
///
/// A request carries no provenance of its own — a tool result is a
/// string and nothing in it says where it came from — but it does carry
/// the CALL the result answers, and a call carries its arguments. So a
/// result whose call passed a path to a tool this program declared is a
/// file with a name, and the user reads that name before deciding. What
/// the model quoted into its own prose still has no name, and the note
/// says so rather than letting the list read as complete.
#[test]
fn a_manifest_names_the_file_a_tool_was_asked_for_and_says_what_it_cannot_name() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);

    send(&mut backend, &a_full_request()).expect("the turn ran");

    let manifest = shown.last();
    let text = manifest.render();
    assert_eq!(manifest.sources.len(), 1, "{:?}", manifest.sources);
    assert_eq!(
        manifest.sources[0].path.to_string_lossy(),
        "/home/michael/.config/nacelle-desktop/x.conf"
    );
    assert!(
        text.contains("/home/michael/.config/nacelle-desktop/x.conf"),
        "the file is not on the screen the user answers: {text}"
    );
    assert!(
        text.contains("quoted, pasted or summarised"),
        "a file list with nothing said about what is missing from it: {text}"
    );
}

/// And the consequence that was the whole point of recording them: layer
/// 4 asks again when a file the user has not answered for turns up.
///
/// `Disclosure::required_for` has always had this rule and it has never
/// been able to fire, because the source list was hard-coded empty —
/// measured, one yes covered every later turn of the session including
/// one carrying a diary the user had never seen a word of.
#[test]
fn a_file_the_user_has_not_answered_for_asks_again() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);

    send(&mut backend, &a_full_request()).expect("the first turn");
    assert_eq!(shown.count(), 1, "the first escalation is always shown");

    // The same file again: nothing new, and layer 4 is not troubled.
    send(&mut backend, &a_full_request()).expect("the second turn");
    assert_eq!(shown.count(), 1, "the same file was disclosed twice");

    // A different one, which the user has never answered for.
    let another = a_conversation_with(ToolCall {
        id: "call_9".to_string(),
        name: "read_config".to_string(),
        input: json!({"path": "/home/michael/.ssh/config"}),
    })
    .with_message(Message::new(
        Role::User,
        vec![Content::ToolResult {
            id: "call_9".to_string(),
            output: "Host git\n  User michael\n".to_string(),
            is_error: false,
        }],
    ));
    send(&mut backend, &another).expect("the third turn");

    assert_eq!(shown.count(), 2, "a file the user never saw went unannounced");
    assert_eq!(shown.last().why, nacelle_ai::Why::UnseenFile);
}

/// The one place the two numbers can differ, pinned to the safe
/// direction. An unsigned thinking block cannot be replayed, so the
/// encoder drops it — but the seal scans and counts it anyway, because
/// what is sealed is the request rather than the subset one encoder
/// happens to emit today. The manifest therefore counts at least what
/// goes, and never less: nothing reaches the socket that the user was
/// not shown a byte for.
#[test]
fn the_manifest_never_counts_less_than_what_reaches_the_socket() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);

    let request = a_full_request().with_message(Message::new(
        Role::Assistant,
        vec![
            Content::Thinking {
                text: "a thought this program cannot prove it did not edit".to_string(),
                signature: None,
            },
            Content::Text("so, about that config".to_string()),
        ],
    ));

    send(&mut backend, &request).expect("the turn ran");

    let sent = wire.sent.lock().unwrap()[0].clone();
    let body: serde_json::Value = serde_json::from_slice(&sent).expect("the body is JSON");
    let went = text_on_the_wire(&body);
    let manifest = shown.last();

    assert!(
        manifest.bytes >= went.len(),
        "{} bytes went and the user was shown {}",
        went.len(),
        manifest.bytes
    );
    assert!(
        !String::from_utf8_lossy(&sent).contains("cannot prove it did not edit"),
        "an unsigned thinking block was sent after all"
    );
}

// ------------------------------------ the strings that travel verbatim

/// One tool call, in a conversation that declares exactly one tool with
/// exactly one argument.
///
/// The declaration is the registry as the seal reads it: a request
/// carries the tools the agent offered, so the seal can tell a name this
/// program put there from a name the model made up without knowing
/// anything about the agent that built the request.
fn a_conversation_with(call: ToolCall) -> Request {
    Request::new(MODEL)
        .with_message(Message::user("look at my config"))
        .with_message(Message::new(Role::Assistant, vec![Content::ToolUse(call)]))
        .with_tool(ToolDeclaration::new(
            "read_config",
            "Read a configuration file and report what is in it.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false
            }),
        ))
}

/// Arguments with a key that is not a literal, which `json!` cannot
/// write.
fn arguments(pairs: &[(&str, serde_json::Value)]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (key, value) in pairs {
        map.insert((*key).to_string(), value.clone());
    }
    serde_json::Value::Object(map)
}

/// The hole this section exists for. A tool name is not scanned, because
/// a marker in place of a name is a call the far model cannot dispatch —
/// but that argument only ever defended the names this program declared.
/// A name the registry never heard of dispatches to nothing either way,
/// so there is nothing left for it to buy, and it is a string the local
/// model wrote from end to end.
#[test]
fn a_tool_name_the_registry_does_not_know_does_not_carry_a_key_off_the_machine() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);

    let outcome = send(
        &mut backend,
        &a_conversation_with(ToolCall {
            id: "call_1".to_string(),
            name: format!("exfil_{KEY}"),
            input: json!({}),
        }),
    );

    // Asserted before the refusal is looked at, so that a run without the
    // guard says what actually happened rather than only that no error
    // came back.
    assert!(!wire.bytes().contains(KEY), "a key left inside a tool name");
    assert_eq!(wire.requests(), 0, "a socket was opened for it");
    let err = outcome.expect_err("a name with a key in it is not a turn that goes");
    // Not even asked about: a manifest for a payload that was never
    // sendable is a question with one honest answer, and asking it
    // teaches the reflex layer 4 exists to interrupt.
    assert_eq!(shown.count(), 0, "the user was asked about an unsendable payload");

    let BackendError::Withheld(tell) = err else {
        panic!("a name that cannot be sent should read as a refusal to send");
    };
    assert!(
        !tell.contains(KEY),
        "the refusal put the secret back on the screen: {tell}"
    );
    assert!(tell.contains("an Anthropic API key"), "{tell}");
}

/// The same hole through the other door. `strings_in` walks a JSON
/// document's values and never its keys, so a key is the one place in a
/// tool call that no layer has ever looked at.
#[test]
fn an_argument_name_the_schema_does_not_declare_does_not_carry_a_key_off_the_machine() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);

    let outcome = send(
        &mut backend,
        &a_conversation_with(ToolCall {
            id: "call_1".to_string(),
            name: "read_config".to_string(),
            input: arguments(&[
                ("path", json!("/home/u/.config/x")),
                (KEY, json!(true)),
            ]),
        }),
    );

    assert!(
        !wire.bytes().contains(KEY),
        "a key left inside an argument name"
    );
    assert_eq!(wire.requests(), 0, "a socket was opened for it");
    let err = outcome.expect_err("an argument name with a key in it is not a turn that goes");
    let BackendError::Withheld(tell) = err else {
        panic!("an argument name that cannot be sent should read as a refusal to send");
    };
    assert!(!tell.contains(KEY), "{tell}");
}

/// And the identifiers, which are echoed so that a result can be matched
/// to its call. Ollama mints one out of the tool's own name when the
/// server sends none, so an id is not a string this program wrote either.
///
/// It is now, and that is the change this test records: the seal
/// numbers the calls in the copy it encodes, so whatever was in the
/// field does not leave and the turn is not lost either. The rule this
/// replaced could only see NAMED shapes in an identifier — the entropy
/// rule had to be kept away from it, or every `toolu_01…` would have
/// ended a session — so a secret with no name went straight through.
#[test]
fn an_identifier_is_replaced_by_one_this_program_wrote() {
    for id in [format!("toolu_{KEY}"), format!("{KEY}-0")] {
        let wire = Wire::ok();
        let shown = Shown::default();
        let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);

        let request = a_conversation_with(ToolCall {
            id: id.clone(),
            name: "read_config".to_string(),
            input: json!({"path": "/home/u/.config/x"}),
        })
        .with_message(Message::new(
            Role::User,
            vec![Content::ToolResult {
                id,
                output: "Theme = crimson\n".to_string(),
                is_error: false,
            }],
        ));

        send(&mut backend, &request).expect("the turn is not lost over an id");

        let sent = wire.bytes();
        assert!(!sent.contains(KEY), "a key left inside a call identifier");
        assert_eq!(wire.requests(), 1);
        // And the conversation is still well formed: the result still
        // names the call it answers.
        let (call, result) = the_two_ids(&wire);
        assert_eq!(call, result, "the result no longer matches its call");
    }
}

/// The `tool_use` id and the `tool_result` id as the endpoint would read
/// them.
fn the_two_ids(wire: &Arc<Wire>) -> (String, String) {
    let sent = wire.sent.lock().unwrap()[0].clone();
    let body: serde_json::Value = serde_json::from_slice(&sent).expect("the body is JSON");
    let mut call = String::new();
    let mut result = String::new();
    for message in body["messages"].as_array().expect("messages") {
        for block in message["content"].as_array().expect("content") {
            match block["type"].as_str() {
                Some("tool_use") => call = block["id"].as_str().unwrap_or_default().to_string(),
                Some("tool_result") => {
                    result = block["tool_use_id"].as_str().unwrap_or_default().to_string()
                }
                _ => {}
            }
        }
    }
    (call, result)
}

/// The other half of the same decision, and the one that keeps it from
/// being a session-ending trap. A small local model invents tool names
/// constantly, and an invented name lands in the history before anything
/// tries to run it. If an unknown name stopped the turn on its own, one
/// hallucination would end the session's ability to reach Claude at all
/// — so the name has to be refused for what is *in* it, not for being
/// unknown.
#[test]
fn a_hallucinated_tool_name_is_not_a_reason_to_stop_the_turn() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);

    send(
        &mut backend,
        &a_conversation_with(ToolCall {
            id: "call_1".to_string(),
            name: "get_weather".to_string(),
            input: json!({"location": "Earth"}),
        }),
    )
    .expect("a name this program does not have is still only a name");

    assert_eq!(wire.requests(), 1);
    assert!(wire.bytes().contains("get_weather"));
    assert!(wire.bytes().contains("location"));
}

/// An identifier is opaque by construction, which is exactly the profile
/// the entropy rule is looking for — so the entropy rule could never be
/// asked about one without ending sessions over `toolu_01…`, and a
/// secret with no named shape sat in that field unread. Renaming the
/// calls closes that without any rule at all: the field carries a
/// counter, so there is nothing in it to judge.
///
/// An opaque id does not end the turn, and it does not travel either.
#[test]
fn an_opaque_identifier_neither_stops_the_turn_nor_travels() {
    const OPAQUE: &str = "toolu_01Qm7Rt2Wb9Ye4Uh6Ij1Ok3Pl5Zx8Cv0N";
    assert!(
        scan(OPAQUE)
            .findings
            .iter()
            .any(|finding| finding.kind == Kind::HighEntropy),
        "this test is measuring nothing: layer 2 says nothing about {OPAQUE}"
    );

    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);

    send(
        &mut backend,
        &a_conversation_with(ToolCall {
            id: OPAQUE.to_string(),
            name: "read_config".to_string(),
            input: json!({"path": "/home/u/.config/x"}),
        }),
    )
    .expect("an opaque identifier is what an identifier looks like");

    assert_eq!(wire.requests(), 1);
    assert!(
        !wire.bytes().contains(OPAQUE),
        "the identifier the model chose reached the socket"
    );
}

/// The argument the module header makes, kept true where it actually
/// holds: a name the registry declared is this program's own constant,
/// nothing read off the machine can be inside it, and the far model
/// dispatches on it. Those travel exactly as they are.
#[test]
fn the_names_the_registry_declared_still_travel_verbatim() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(Trigger::UserAsked, shown.agreeing()), &wire);

    send(
        &mut backend,
        &a_conversation_with(ToolCall {
            id: "call_1".to_string(),
            name: "read_config".to_string(),
            input: json!({"path": "/home/u/.config/x"}),
        }),
    )
    .expect("the turn ran");

    let sent = wire.bytes();
    assert!(sent.contains("read_config"));
    assert!(sent.contains("path"));
    // The identifier is NOT on this list any more. It is not a string
    // this program wrote — Ollama builds one out of whatever the local
    // model called the tool — so it is renumbered rather than trusted.
    let (call, result) = the_two_ids(&wire);
    assert_eq!(call, "toolu_0001");
    assert_eq!(result, "");
}
