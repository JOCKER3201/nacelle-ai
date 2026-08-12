//! Adversarial probes against the door in `supervise::seal`.
//!
//! Same shape as `seal.rs`: every case drives the real Anthropic backend
//! over a transport that is a `Vec<u8>`, and then reads THE BYTES THAT
//! REACHED THE SOCKET. Nothing here touches a network.
//!
//! **These were measurements and are now a regression suite.** Every
//! test below was written to pass while its hole was open, and the
//! comment above each one says what was measured going out. The
//! assertions have been turned round: they now say the hole is shut, and
//! the sentence that described the leak is kept so that a reader can see
//! what the test is defending and what it cost to close.

use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nacelle_ai::backend::anthropic::{self, Anthropic, HttpResponse, Retry, Transport};
use nacelle_ai::credentials::Credential;
use nacelle_ai::{
    Backend, BackendError, Consent, Content, Flow, Manifest, Message, Policy, Remote, Request,
    Seal, ToolCall, ToolDeclaration, Trigger,
};
use serde_json::json;

const KEY: &str = "sk-ant-api03-Zx8Qv2Lm4Np7Rt1Ws9Yb3Cd6Ef0Gh5Ij8Kl2Mn4Op6Qr8St0Uv2Wx4Yz";
const MODEL: &str = anthropic::DEFAULT_MODEL.id;

// ---------------------------------------------------------------- stubs

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

    fn requests(&self) -> usize {
        self.sent.lock().unwrap().len()
    }

    fn bytes(&self) -> String {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .map(|body| String::from_utf8_lossy(body).into_owned())
            .collect()
    }

    fn last_len(&self) -> usize {
        self.sent.lock().unwrap().last().map_or(0, Vec::len)
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

#[derive(Clone, Default)]
struct Shown(Arc<Mutex<Vec<Manifest>>>);

impl Shown {
    fn count(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    fn last(&self) -> Manifest {
        self.0.lock().unwrap().last().cloned().expect("a manifest")
    }

    fn agreeing(&self) -> impl FnMut(&Manifest) -> Consent + Send {
        let seen = Arc::clone(&self.0);
        move |manifest: &Manifest| {
            seen.lock().unwrap().push(manifest.clone());
            Consent::Send
        }
    }

    fn refusing(&self) -> impl FnMut(&Manifest) -> Consent + Send {
        let seen = Arc::clone(&self.0);
        move |manifest: &Manifest| {
            seen.lock().unwrap().push(manifest.clone());
            Consent::refuse("no")
        }
    }
}

fn wired(seal: Seal, wire: &Arc<Wire>) -> Anthropic<Held> {
    Anthropic::with_transport(
        Credential::api_key("sk-ant-test-credential-value"),
        seal,
        Held(Arc::clone(wire)),
    )
    .with_retry(Retry {
        attempts: 1,
        backoff: Duration::ZERO,
        cap: Duration::ZERO,
    })
}

fn seal_with(discloser: impl FnMut(&Manifest) -> Consent + Send + 'static) -> Seal {
    Seal::new(
        anthropic::NAME,
        Policy::new(Remote::Ready),
        Trigger::UserAsked,
        discloser,
    )
}

fn send(backend: &mut Anthropic<Held>, request: &Request) -> Result<(), BackendError> {
    backend.send(request, &mut |_| Flow::Continue)
}

// ------------------------------------------------------- probe 1: signature

/// A thinking block's `signature` was emitted verbatim by the encoder
/// and walked by nothing: not `each_text` (so no layer 2, no layer 3,
/// and no byte of it on the manifest) and not `unsendable`. Measured:
/// `key on the wire: true`, `marker on the wire: false`, 354 bytes of
/// body against a 45-byte manifest.
///
/// It cannot be redacted — the endpoint verifies it and rejects a block
/// that was edited — so it is read for named shapes and the turn stops
/// on a hit, which is what every other unmarkable string here does.
#[test]
fn a_key_in_a_signature_does_not_reach_the_socket() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(shown.agreeing()), &wire);

    let request = Request::new(MODEL)
        .with_message(Message::user("what did you conclude?"))
        .with_message(Message::new(
            nacelle_ai::Role::Assistant,
            vec![Content::Thinking {
                text: "I looked at the file.".to_string(),
                signature: Some(KEY.to_string()),
            }],
        ));

    let outcome = send(&mut backend, &request);

    let sent = wire.bytes();
    println!("--- probe 1 ---");
    println!("outcome: {outcome:?}");
    println!("requests: {}", wire.requests());
    println!("key on the wire: {}", sent.contains(KEY));
    assert!(!sent.contains(KEY), "a key left inside a signature");
    assert_eq!(wire.requests(), 0, "a socket was opened for it");
}

/// The manifest promises "never smaller than what goes", and with a
/// signature present it was smaller: measured, `manifest says: 12
/// bytes`, `signature alone: 4000 bytes`, `wire body: 4252 bytes`.
///
/// An ordinary signature carries no credential shape, so the turn goes —
/// and the number the user reads has to cover it.
#[test]
fn the_manifest_counts_a_signature_it_cannot_edit() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(shown.agreeing()), &wire);

    let filler = "x".repeat(4000);
    let request = Request::new(MODEL)
        .with_message(Message::user("hello"))
        .with_message(Message::new(
            nacelle_ai::Role::Assistant,
            vec![Content::Thinking {
                text: "short".to_string(),
                signature: Some(filler.clone()),
            }],
        ));

    send(&mut backend, &request).expect("the turn ran");

    let manifest = shown.last();
    println!("--- probe 2 ---");
    println!("manifest says: {} bytes", manifest.bytes);
    println!("signature alone: {} bytes", filler.len());
    println!("wire body: {} bytes", wire.last_len());
    assert!(
        manifest.bytes >= filler.len(),
        "the manifest still undercounts: {} against {}",
        manifest.bytes,
        filler.len()
    );
    assert!(wire.bytes().contains(&filler));
}

// -------------------------------------- probe 3: a declaration's schema keys

/// `unsendable` read a tool DECLARATION's name and a tool CALL's
/// argument names, but never a declaration's own schema keys —
/// `strings_in` walks values only, and the declaration loop only looked
/// at `tool.name`. Measured: `key on the wire: true`, against a control
/// (probe 4) in which the same key as the declaration's NAME held the
/// turn. The asymmetry was introduced by the patch that started reading
/// a call's argument names.
#[test]
fn a_schema_property_name_does_not_carry_a_key_to_the_socket() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(shown.agreeing()), &wire);

    let request = Request::new(MODEL)
        .with_message(Message::user("go"))
        .with_tool(ToolDeclaration::new(
            "read_file",
            "read a file",
            json!({
                "type": "object",
                "properties": { KEY: { "type": "string" } },
            }),
        ));

    let outcome = send(&mut backend, &request);

    let sent = wire.bytes();
    println!("--- probe 3 ---");
    println!("outcome: {outcome:?}");
    println!("requests: {}", wire.requests());
    println!("key on the wire: {}", sent.contains(KEY));
    assert!(!sent.contains(KEY), "a key left inside a declared schema");
    assert_eq!(wire.requests(), 0, "a socket was opened for it");
}

// ------------------------------------------- probe 4: a call to a known tool

/// `unsendable` only reads a CALL's name when the registry does not know
/// it. Two calls that both name a declared tool are compared by string
/// equality, so this is a control: it should be clean.
#[test]
fn probe_a_declared_call_name_is_not_read() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(shown.agreeing()), &wire);

    // The declared name IS the key. `unsendable`'s declaration loop
    // should catch this one — the control for probe 3.
    let request = Request::new(MODEL)
        .with_message(Message::user("go"))
        .with_tool(ToolDeclaration::new(KEY, "a tool", json!({})));

    let outcome = send(&mut backend, &request);
    println!("--- probe 4 ---");
    println!("outcome: {outcome:?}");
    println!("requests: {}", wire.requests());
    assert_eq!(wire.requests(), 0, "a declared name with a key in it went");
}

// -------------------------------------------- probe 5: refusal, then a retry

/// Does "no" hold across turns, or only for the turn it was said on?
#[test]
fn probe_refusal_then_another_turn() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(shown.refusing()), &wire);

    let first = send(&mut backend, &Request::new(MODEL).with_message(Message::user("one")));
    let second = send(&mut backend, &Request::new(MODEL).with_message(Message::user("two")));

    println!("--- probe 5 ---");
    println!("first: {first:?}");
    println!("second: {second:?}");
    println!("manifests shown: {}", shown.count());
    println!("requests: {}", wire.requests());
    assert_eq!(wire.requests(), 0);
}

// ------------------------------- probe 6: one yes, then everything else goes

/// **This one still passes, and it is the honest half of the fix.**
///
/// The measurement was: after a single consent the disclosure is
/// `shown`, and a seal-built `Outgoing` never had sources — so
/// `Disclosure::required_for` returned `None` for every later turn and
/// layer 4 was never asked again. The source list is now built from the
/// request (see `a_manifest_names_the_file_a_tool_was_asked_for…`), so
/// the "something new" rule can fire; but it fires on FILES, and prose
/// the user typed or the model wrote is not a file.
///
/// So a second turn of plain conversation still goes without a second
/// manifest, and that is deliberate — a manifest before every turn is
/// one that gets clicked through, which is worse than none. What is
/// asserted here is the shape of the remaining exposure, spelled out so
/// that nobody has to discover it: **if a payload carries something
/// private that never came from a named file, the user is asked once a
/// session and not again.**
#[test]
fn one_yes_still_covers_every_later_turn_of_prose() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(shown.agreeing()), &wire);

    send(&mut backend, &Request::new(MODEL).with_message(Message::user("hello")))
        .expect("first turn");
    let asked_after_first = shown.count();

    // A brand-new payload the user has never seen a word of.
    send(
        &mut backend,
        &Request::new(MODEL).with_message(Message::user(
            "here is my diary: 1998-04-02, the diagnosis was confirmed",
        )),
    )
    .expect("second turn");

    println!("--- probe 6 ---");
    println!("manifests after first turn: {asked_after_first}");
    println!("manifests after second turn: {}", shown.count());
    println!("requests: {}", wire.requests());
    println!(
        "diary on the wire: {}",
        wire.bytes().contains("the diagnosis was confirmed")
    );
    assert_eq!(shown.count(), 1, "layer 4 asked more than once");
    assert_eq!(wire.requests(), 2);
}

// --------------------------------- probe 7: a key split across two blocks

/// Layer 2 runs per string. Two blocks, one key.
#[test]
fn probe_a_key_split_across_two_blocks() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(shown.agreeing()), &wire);

    let (head, tail) = KEY.split_at(13);
    let request = Request::new(MODEL).with_message(Message::new(
        nacelle_ai::Role::User,
        vec![
            Content::Text(format!("the key begins {head}")),
            Content::Text(tail.to_string()),
        ],
    ));

    send(&mut backend, &request).expect("the turn ran");

    let sent = wire.bytes();
    println!("--- probe 7 ---");
    println!("head on the wire: {}", sent.contains(head));
    println!("tail on the wire: {}", sent.contains(tail));
    println!("whole key contiguous: {}", sent.contains(KEY));
    // The defence held: the tail is long enough for the entropy rule,
    // so it went. Recorded as a control, not as a hole.
    assert!(!sent.contains(tail), "the tail of the key survived layer 2");
}

// ------------------------- probe 8: a tool call id that is a declared name

/// Ollama builds an id out of the tool's own name. `shape_in_identifier`
/// drops `HighEntropy` findings. What else does it drop?
#[test]
fn probe_an_identifier_holding_a_labelled_value() {
    let wire = Wire::ok();
    let shown = Shown::default();
    let mut backend = wired(seal_with(shown.agreeing()), &wire);

    let request = Request::new(MODEL)
        .with_message(Message::user("go"))
        .with_message(Message::new(
            nacelle_ai::Role::Assistant,
            vec![Content::ToolUse(ToolCall {
                // No named provider shape, no prefix — just a long
                // opaque string that happens to be somebody's data.
                id: "toolu_01_michael_furtak_1981_04_02_krakow".to_string(),
                name: "read_file".to_string(),
                input: json!({ "path": "/etc/hosts" }),
            })],
        ))
        .with_message(Message::new(
            nacelle_ai::Role::User,
            vec![Content::ToolResult {
                id: "toolu_01_michael_furtak_1981_04_02_krakow".to_string(),
                output: "ok".to_string(),
                is_error: false,
            }],
        ))
        .with_tool(ToolDeclaration::new(
            "read_file",
            "read",
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        ));

    let outcome = send(&mut backend, &request);
    let sent = wire.bytes();
    println!("--- probe 8 ---");
    println!("outcome: {outcome:?}");
    println!(
        "id on the wire: {}",
        sent.contains("michael_furtak_1981_04_02_krakow")
    );
}

// ------------------------------------ probe 9: a stop that arrives too late

/// `Cancel` was read in exactly two places: at the head of the worker
/// loop, before the turn starts, and inside the sink. The sink is not
/// called until the response is being decoded — which is AFTER
/// `transport.post`. Everything in between (layer 2, layer 3's whole
/// turn against the local model, layer 4's manifest) was a window in
/// which a stop was not observed and the payload left anyway. Measured:
/// `turn outcome: Failed(Cancelled)` on the interface and `requests that
/// reached the socket: 1` on the transport — the user was told the turn
/// had stopped, and the bytes had gone.
///
/// The reviewer here fires the cancel, standing in for the user pressing
/// escape while layer 3 is talking to Ollama — seconds, in a real
/// session.
#[test]
fn a_stop_during_layer_three_stops_the_bytes() {
    use nacelle_ai::agent::registry::{Effect, ToolOutput, ToolRegistry};
    use nacelle_ai::{Agent, AgentEvent, Cancel, Review, Reviewer, Worker};

    struct NoTools;
    impl ToolRegistry for NoTools {
        fn declarations(&self) -> Vec<ToolDeclaration> {
            Vec::new()
        }
        fn effect(&self, _call: &ToolCall) -> Effect {
            Effect::Read
        }
        fn invoke(&mut self, _call: &ToolCall) -> ToolOutput {
            ToolOutput::error("no tools")
        }
    }

    /// Layer 3, which takes its time — and while it does, the user
    /// presses stop.
    struct StoppedMidReview(Arc<Mutex<Option<Cancel>>>);
    impl Reviewer for StoppedMidReview {
        fn review(&mut self, _payload: &str) -> Review {
            if let Some(cancel) = self.0.lock().unwrap().as_ref() {
                cancel.cancel();
            }
            Review::nothing()
        }
    }

    let wire = Wire::ok();
    let shown = Shown::default();
    // Filled in after spawn: the reviewer needs the worker's own stop
    // button, which does not exist until the worker does.
    let button: Arc<Mutex<Option<Cancel>>> = Arc::new(Mutex::new(None));

    let seal = seal_with(shown.agreeing()).with_reviewer(StoppedMidReview(Arc::clone(&button)));
    let agent = Agent::new(Box::new(wired(seal, &wire)), Box::new(NoTools), MODEL);
    let (worker, inbox) = Worker::spawn(agent).expect("a worker");
    *button.lock().unwrap() = Some(worker.cancel_handle());

    worker.ask("here are my notes, tell me what you think").expect("queued");

    let mut outcome = String::new();
    while let Ok(event) = inbox.recv() {
        let done = matches!(event, AgentEvent::Finished(_) | AgentEvent::Failed(_));
        if done {
            outcome = format!("{event:?}");
            break;
        }
    }

    println!("--- probe 9 ---");
    println!("turn outcome: {outcome}");
    println!("manifests shown: {}", shown.count());
    println!("requests that reached the socket: {}", wire.requests());
    println!(
        "the question is on the wire: {}",
        wire.bytes().contains("here are my notes")
    );
    assert_eq!(
        wire.requests(),
        0,
        "a cancelled turn still posted: the interface said stopped and the bytes went"
    );
    assert!(
        outcome.contains("Cancelled"),
        "a stopped turn should read as stopped, not as a backend failure: {outcome}"
    );
    drop(worker);
}

// ---------------------- probe 10: a stop while the manifest is on screen

/// The other half of the same window, and the one the adversary listed
/// as reasoned about rather than measured.
///
/// `ChannelDiscloser::disclose` waited in `answer.recv()` with no
/// timeout: if the interface holds the `PendingDisclosure` — which is
/// what a window showing a manifest does — the sender is alive, the
/// receive never returns, and the worker is inside the door. A stop is
/// not read, and `Worker::shutdown` joins that thread. So this measures
/// two things at once: that pressing stop with a manifest open ends the
/// turn without sending, and that shutting the worker down afterwards
/// returns rather than hanging.
#[test]
fn a_stop_while_the_manifest_is_open_stops_the_turn_and_the_worker() {
    use nacelle_ai::agent::registry::{Effect, ToolOutput, ToolRegistry};
    use nacelle_ai::{over_channel, Agent, AgentEvent, Worker};

    struct NoTools;
    impl ToolRegistry for NoTools {
        fn declarations(&self) -> Vec<ToolDeclaration> {
            Vec::new()
        }
        fn effect(&self, _call: &ToolCall) -> Effect {
            Effect::Read
        }
        fn invoke(&mut self, _call: &ToolCall) -> ToolOutput {
            ToolOutput::error("no tools")
        }
    }

    let wire = Wire::ok();
    let (discloser, manifests) = over_channel();
    let seal = seal_with_discloser(discloser);
    let agent = Agent::new(Box::new(wired(seal, &wire)), Box::new(NoTools), MODEL);
    let (worker, inbox) = Worker::spawn(agent).expect("a worker");

    worker.ask("here are my notes").expect("queued");

    // The interface receives the manifest and holds it — a window is on
    // screen — and then the user presses stop instead of answering.
    let pending = manifests
        .recv_timeout(Duration::from_secs(5))
        .expect("a manifest reached the interface");
    worker.cancel();

    let mut outcome = String::new();
    while let Ok(event) = inbox.recv_timeout(Duration::from_secs(5)) {
        if matches!(event, AgentEvent::Finished(_) | AgentEvent::Failed(_)) {
            outcome = format!("{event:?}");
            break;
        }
    }

    println!("--- probe 10 ---");
    println!("turn outcome: {outcome}");
    println!("requests that reached the socket: {}", wire.requests());
    assert_eq!(wire.requests(), 0, "a turn stopped at the manifest still posted");
    assert!(!outcome.is_empty(), "the turn never ended");

    // Still holding the question the user never answered, and the
    // worker still shuts down.
    drop(pending);
    worker.shutdown();
}

fn seal_with_discloser(discloser: impl nacelle_ai::Discloser + Send + 'static) -> Seal {
    Seal::new(
        anthropic::NAME,
        Policy::new(Remote::Ready),
        Trigger::UserAsked,
        discloser,
    )
}
