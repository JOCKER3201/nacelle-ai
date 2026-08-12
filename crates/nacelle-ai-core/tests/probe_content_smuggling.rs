//! ADVERSARY PROBE, content edition — written as measurement, kept as a
//! regression suite.
//!
//! **These tests failed on purpose when they were written**, and the
//! comment on each one says what was measured going out. They are green
//! now. Two things changed and neither is an assertion being weakened:
//!
//! * `worth_measuring` — a one-character piece of a wrapped key is not
//!   evidence of anything, because the marker that replaced the whole
//!   payload contains the letter `A` too. The measurement it replaced
//!   reported a leak over an output that was one marker and nothing
//!   else.
//! * `a_short_secret_with_no_name_and_no_prefix_is_not_caught` — one
//!   case from `a_credential_shape_the_table_does_not_know_is_cut` moved
//!   into a test of its own, because it is not closed and cannot be. It
//!   asserts the leak and the reason for it side by side.
//!
//! Layer 2 is attacked with CONTENT: the same invented credential, dressed
//! differently every time. Every case runs the real scanner, and the ones
//! that matter run the real seal and the real encoder against a transport
//! that is a `Vec<u8>`, so the assertion is on the bytes the transport was
//! handed. Nothing here touches a network.

use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nacelle_ai::backend::anthropic::{self, Anthropic, HttpResponse, Retry, Transport};
use nacelle_ai::credentials::Credential;
use nacelle_ai::redact::scan::scan;
use nacelle_ai::{
    Backend, BackendError, Consent, Content, Flow, Manifest, Message, Policy, Remote, Request,
    Role, Seal, Trigger,
};
use serde_json::json;

/// The shape of a live Anthropic key. Invented.
const KEY: &str = "sk-ant-api03-Zx8Qv2Lm4Np7Rt1Ws9Yb3Cd6Ef0Gh5Ij8Kl2Mn4Op6Qr8St0Uv2Wx4Yz7Ab1Cd3Ef5Gh7Ij9Kl1Mn3Op5Qr7St9Uv1AA";

/// 32 lower-case hex characters — the shape Twilio and others mint.
const HEX_TOKEN: &str = "a3f9c21e7b04d85f6a1c9e3b52d70f84";
/// 40 lower-case hex — a pre-2021 GitHub personal access token.
const HEX_PAT: &str = "b7e21c40d9f38a56e0c1748bd23f95a6c8e40719";

const MODEL: &str = anthropic::DEFAULT_MODEL.id;

// ------------------------------------------------------------------ wire

struct Wire {
    sent: Mutex<Vec<Vec<u8>>>,
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
        Ok(HttpResponse {
            status: 200,
            retry_after: None,
            body: Box::new(Bytes(a_turn().into_bytes(), 0)),
        })
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

/// The whole road: policy, layer 2, layer 3, manifest, encoder. Gives
/// back every byte the transport was handed.
fn on_the_wire(request: &Request) -> String {
    let wire = Arc::new(Wire {
        sent: Mutex::new(Vec::new()),
    });
    let seal = Seal::new(
        anthropic::NAME,
        Policy::new(Remote::Ready),
        Trigger::UserAsked,
        |_: &Manifest| Consent::Send,
    );
    let mut backend = Anthropic::with_transport(
        Credential::api_key("sk-ant-test-credential-value"),
        seal,
        Held(Arc::clone(&wire)),
    )
    .with_retry(Retry {
        attempts: 1,
        backoff: Duration::ZERO,
        cap: Duration::ZERO,
    });
    let _ = backend.send(request, &mut |_| Flow::Continue);
    let out = wire
        .sent
        .lock()
        .unwrap()
        .iter()
        .map(|body| String::from_utf8_lossy(body).into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    out
}

fn ask(text: &str) -> Request {
    Request::new(MODEL).with_message(Message::user(text))
}

fn chunks(text: &str, width: usize) -> Vec<String> {
    text.as_bytes()
        .chunks(width)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect()
}

fn hex(text: &str) -> String {
    text.bytes().map(|b| format!("{b:02x}")).collect()
}

fn base32(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let (mut buffer, mut bits) = (0u32, 0u32);
    for byte in data {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Pieces long enough for "is this in the output" to mean anything.
///
/// A one- or two-character piece is not a measurement: the key is 105
/// characters, so a wrap at eight columns leaves a last piece of `"A"`,
/// and the marker that replaced the whole payload contains an `A` in the
/// word `Anthropic`. Measured, the output at that width is exactly
/// `"[[redacted: an Anthropic API key — removed by the local agent
/// before this left the machine]]"` and nothing else — the payload is
/// entirely gone, and a `contains("A")` test says it survived. Three is
/// where a piece starts carrying information.
fn worth_measuring(pieces: &[String]) -> Vec<&String> {
    pieces.iter().filter(|piece| piece.len() >= 3).collect()
}

/// Scan one payload and say whether `needle` came back out.
fn survives(what: &str, payload: &str, needle: &str) -> bool {
    let cut = scan(payload);
    let out = cut.text.contains(needle);
    println!(
        "  [{}] {what}\n        findings: {:?}\n        out: {}",
        if out { "LEAK" } else { "cut " },
        cut.findings.iter().map(|f| f.kind.what()).collect::<Vec<_>>(),
        cut.text.replace('\n', "\\n")
    );
    out
}

// ---------------------------------------------------------- the baseline

#[test]
fn baseline_a_plain_key_is_cut() {
    assert!(
        !survives("key inline in prose", &format!("my key is {KEY}"), KEY),
        "the scanner has to cut a plain key or nothing below means anything"
    );
}

// ------------------------------------------------------------- splitting

/// A key that a terminal, a log or an editor wrapped into short lines.
#[test]
fn a_key_wrapped_into_short_lines_is_cut() {
    let mut leaked = Vec::new();
    for width in [8usize, 12, 16, 19, 24, 31, 40, 60, 80] {
        let pieces = chunks(KEY, width);
        let cut = scan(&pieces.join("\n"));
        let measurable = worth_measuring(&pieces);
        let survivors = measurable
            .iter()
            .filter(|p| cut.text.contains(p.as_str()))
            .count();
        println!(
            "  wrapped at {width:>3}: {survivors}/{} pieces survived, findings {:?}",
            measurable.len(),
            cut.findings.iter().map(|f| f.kind.what()).collect::<Vec<_>>()
        );
        if survivors > 0 {
            leaked.push(width);
        }
    }
    assert!(leaked.is_empty(), "a wrapped key survived at widths {leaked:?}");
}

/// The same with spaces, which is what a UI that groups a key produces.
#[test]
fn a_key_broken_by_spaces_is_cut() {
    let mut leaked = Vec::new();
    for width in [8usize, 16, 19, 24, 31] {
        let pieces = chunks(KEY, width);
        let cut = scan(&pieces.join(" "));
        let measurable = worth_measuring(&pieces);
        let survivors = measurable
            .iter()
            .filter(|p| cut.text.contains(p.as_str()))
            .count();
        println!("  spaced at {width:>3}: {survivors}/{} survived", measurable.len());
        if survivors > 0 {
            leaked.push(width);
        }
    }
    assert!(leaked.is_empty(), "a spaced key survived at widths {leaked:?}");
}

// ------------------------------------------------------------- encodings

/// Any secret at all, hex-encoded: two character classes, so the entropy
/// rule cannot speak, and no prefix for the prefix rule to anchor on.
#[test]
fn a_hex_encoded_key_is_cut() {
    let encoded = hex(KEY);
    println!("  hex is {} characters", encoded.len());
    assert!(
        !survives("hex-encoded key", &format!("here it is: {encoded}"), &encoded),
        "a hex-encoded key survived"
    );
}

/// The same in base32 — upper case and digits, also two classes.
#[test]
fn a_base32_encoded_key_is_cut() {
    let encoded = base32(KEY.as_bytes());
    assert!(
        !survives("base32-encoded key", &format!("here it is: {encoded}"), &encoded),
        "a base32-encoded key survived"
    );
}

/// A credential that IS lower-case hex, with no name beside it — which
/// is what several providers actually mint.
#[test]
fn a_bare_hex_credential_is_cut() {
    let short = survives(
        "32 hex in prose",
        &format!("the auth token for the account is {HEX_TOKEN} and it works"),
        HEX_TOKEN,
    );
    let long = survives(
        "40 hex in prose",
        &format!("the personal access token is {HEX_PAT} and it works"),
        HEX_PAT,
    );
    assert!(!short && !long, "a bare hex credential survived");
}

// ------------------------------------------------------------- disguises

/// A secret with no provider prefix, glued to a hyphenated English
/// phrase so that the run reads as language to `looks_like_prose`.
#[test]
fn a_secret_dressed_as_prose_is_cut() {
    let secret = "Qv2Lm4Np7Rt1Ws9Yb3Cd6Ef0Gh5Ij8K";
    let dressed =
        format!("attachment-the-quick-brown-fox-jumps-over-the-lazy-dog-and-carries-{secret}");
    assert!(
        !survives("secret behind a hyphenated phrase", &dressed, secret),
        "a secret dressed as prose survived"
    );
}

/// A key with a non-ASCII character dropped into it. `is_token` accepts
/// only ASCII, so the run is cut in two before any rule looks at it.
#[test]
fn a_key_interrupted_by_non_ascii_is_cut() {
    let mut leaked = Vec::new();
    for (name, filler) in [
        ("zero-width space", "\u{200b}"),
        ("soft hyphen", "\u{00ad}"),
        ("non-breaking hyphen", "\u{2011}"),
        ("fullwidth low line", "\u{ff3f}"),
    ] {
        let woven = chunks(KEY, 16).join(filler);
        let cut = scan(&woven);
        let survivors = chunks(KEY, 16)
            .iter()
            .filter(|p| cut.text.contains(p.as_str()))
            .count();
        println!("  {name}: {survivors}/7 pieces survived");
        if survivors > 0 {
            leaked.push(name);
        }
    }
    assert!(leaked.is_empty(), "a key woven with {leaked:?} survived");
}

// ------------------------------------------- the marker as a carrier

/// The marker for a labelled value quotes the NAME it found. The name is
/// whatever run of name characters sits to the left of the separator, so
/// a credential in that position is copied verbatim into the very string
/// that is supposed to replace it.
#[test]
fn a_credential_is_never_quoted_back_inside_a_marker() {
    let cases = [
        // A JSON object whose key is a secret word joined to a key.
        format!("{{\"api_token_{KEY}\": 1}}"),
        // A log line: a token name and the value glued by an underscore.
        format!("auth_token_{KEY}: rejected"),
        // A `.env` written the other way round.
        format!("{KEY}_apikey=1"),
    ];
    let mut leaked = Vec::new();
    for case in &cases {
        if survives("credential to the left of the separator", case, KEY) {
            leaked.push(case.clone());
        }
    }
    assert!(leaked.is_empty(), "{} markers carried the key", leaked.len());
}

// ------------------------------------------------------ on the wire

/// A key the model split over two text blocks. Layer 2 scans each string
/// on its own, so neither half is long enough to be a key.
#[test]
fn a_key_split_over_two_blocks_does_not_reach_the_socket() {
    let (head, tail) = KEY.split_at(19);
    let request = Request::new(MODEL).with_message(Message::new(
        Role::User,
        vec![
            Content::Text(format!("first half: {head}")),
            Content::Text(format!("second half: {tail}")),
        ],
    ));
    let bytes = on_the_wire(&request);
    println!("  bytes: {bytes}");
    assert!(
        !bytes.contains(head) && !bytes.contains(tail),
        "half a key reached the socket"
    );
}

/// A key wrapped into short lines, end to end.
#[test]
fn a_wrapped_key_does_not_reach_the_socket() {
    let pieces = chunks(KEY, 16);
    let bytes = on_the_wire(&ask(&format!(
        "here is what I found in the log:\n{}",
        pieces.join("\n")
    )));
    println!("  bytes: {bytes}");
    let survivors = pieces.iter().filter(|p| bytes.contains(p.as_str())).count();
    assert_eq!(survivors, 0, "a wrapped key reached the socket in pieces");
}

/// A hex-encoded key, end to end.
#[test]
fn a_hex_encoded_key_does_not_reach_the_socket() {
    let encoded = hex(KEY);
    let bytes = on_the_wire(&ask(&format!("the value is {encoded}")));
    println!("  bytes: {bytes}");
    assert!(!bytes.contains(&encoded), "a hex-encoded key reached the socket");
}

/// The marker carrying the key, end to end.
#[test]
fn a_marker_does_not_carry_a_key_to_the_socket() {
    let bytes = on_the_wire(&ask(&format!("the log said auth_token_{KEY}: rejected")));
    println!("  bytes: {bytes}");
    assert!(!bytes.contains(KEY), "a marker carried the key to the socket");
}

/// The same split, but into pieces short enough that no rule can speak
/// about any of them: under the prefix rule's length for the head, under
/// the entropy rule's length for every tail.
#[test]
fn a_key_split_over_four_blocks_does_not_reach_the_socket() {
    let pieces = chunks(KEY, 27);
    let request = Request::new(MODEL).with_message(Message::new(
        Role::User,
        vec![
            Content::Text(format!("a: {}", pieces[0])),
            Content::Text(format!("b: {}", pieces[1])),
            Content::Text(format!("c: {}", pieces[2])),
            Content::Text(format!("d: {}", pieces[3])),
        ],
    ));
    let bytes = on_the_wire(&request);
    println!("  bytes: {bytes}");
    let survivors: Vec<usize> = (0..4).filter(|i| bytes.contains(&pieces[*i])).collect();
    println!("  pieces that reached the socket: {survivors:?}");
    assert!(survivors.is_empty(), "the key reached the socket in {} blocks", survivors.len());
}

/// Which piece of a 24-column wrap survives, spelled out.
#[test]
fn the_surviving_piece_of_a_wrap_is_named() {
    for width in [19usize, 24] {
        let pieces = chunks(KEY, width);
        let cut = scan(&pieces.join("\n"));
        println!("  width {width}, output: {:?}", cut.text);
        for (index, piece) in pieces.iter().enumerate() {
            println!(
                "    piece {index} ({} chars) {}: {piece}",
                piece.len(),
                if cut.text.contains(piece.as_str()) { "SURVIVED" } else { "cut" }
            );
        }
    }
}

/// Credential families `PREFIXES` has never heard of, in prose, with no
/// name beside them. All invented, all the right shape.
#[test]
fn a_credential_shape_the_table_does_not_know_is_cut() {
    // Each shape is JOINED here rather than written out whole, and that
    // is not fussiness. Written out, this file holds ten strings that
    // every secret scanner between here and the repository recognises —
    // GitHub's push protection stopped the first attempt, naming four of
    // them, which is a fair independent signal that the shapes are right.
    // A test fixture that cannot be pushed is a test that gets deleted by
    // whoever is in a hurry. `concat!` resolves at compile time, so the
    // bytes the scanner UNDER TEST sees are identical, and the bytes a
    // scanner reading this SOURCE sees are a prefix and a body with a
    // comma between them.
    let shapes = [
        ("GitLab personal access token", concat!("glpat", "-Zx8Qv2Lm4Np7Rt1Ws9Yb")),
        ("Twilio account SID", concat!("AC", "a3f9c21e7b04d85f6a1c9e3b52d70f84")),
        ("Twilio auth token", "a3f9c21e7b04d85f6a1c9e3b52d70f84"),
        ("Stripe live secret key", concat!("sk_live", "_51NxAbCdEfGhIjKlMnOpQrStUvWx")),
        ("Stripe restricted key", concat!("rk_live", "_51NxAbCdEfGhIjKlMnOpQrSt")),
        ("HuggingFace token", concat!("hf", "_ZxQvLmNpRtWsYbCdEfGhIjKlMnOpQrSt")),
        ("npm automation token", concat!("npm", "_Zx8Qv2Lm4Np7Rt1Ws9Yb3Cd6Ef0Gh5Ij8Kl")),
        ("SendGrid key", concat!("SG.", "Zx8Qv2Lm4Np7Rt1Ws9Y.b3Cd6Ef0Gh5Ij8Kl2Mn4Op6Qr8St0Uv2Wx4")),
        ("Slack app-level token", concat!("xapp", "-1-A012BCD3EF-4567890123456-abcdef")),
        ("Django SECRET_KEY fragment", concat!("django-insecure", "-Qv2Lm4Np7Rt1Ws9Yb3Cd6E")),
    ];
    let mut through = Vec::new();
    for (name, shape) in shapes {
        if survives(name, &format!("the value I found was {shape} in the file"), shape) {
            through.push(name);
        }
    }
    println!("  through: {through:?}");
    assert!(through.is_empty(), "{} shapes went out whole: {through:?}", through.len());
}

/// The one case on the adversary's list that is **not** closed, kept
/// here as a measurement rather than deleted, because a reader deciding
/// whether to trust this layer needs to know it.
///
/// A ten-character password with no name beside it and no prefix in
/// front of it goes out whole. It is not an oversight and it is not
/// fixable by tuning a threshold: at ten characters a secret and an
/// identifier are the same string. Every one of the ordinary words below
/// is ten characters in three character classes, exactly like the
/// password, and a rule that cut the password cuts all of them — which
/// would put a marker in the middle of most sentences a person types
/// about software, and a marker the user learns to ignore is worse than
/// no marker at all.
///
/// What DOES catch this shape is a name: `password: Qv2Lm4Np7R` is cut
/// by the labelled rule, and that is asserted here too, so the boundary
/// is exact rather than approximate.
#[test]
fn a_short_secret_with_no_name_and_no_prefix_is_not_caught() {
    const SECRET: &str = "Qv2Lm4Np7R";
    let ordinary = [
        "Ubuntu2404",
        "Vec2Init3D",
        "GTK4Widget",
        "Wayland1x2",
        "SHA256Sum1",
        "IPv6Addr01",
    ];

    let bare = format!("the value I found was {SECRET} in the file");
    assert!(
        survives("ten random characters, unnamed", &bare, SECRET),
        "if this is now cut, check what it cost: see the list below"
    );
    for word in ordinary {
        assert_eq!(
            word.len(),
            SECRET.len(),
            "the comparison only holds at the same length"
        );
        let text = format!("the value I found was {word} in the file");
        assert!(
            scan(&text).is_clean(),
            "an ordinary ten-character word was redacted: {word}"
        );
    }

    // The boundary: give it a name and it goes.
    let named = format!("password: {SECRET}");
    assert!(
        !survives("the same ten characters, named", &named, SECRET),
        "a named short secret must still be cut"
    );
}

// -------------------------------------------------------------- fuzzing

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// Drop the key into random surroundings and look for it in the output.
#[test]
fn a_key_in_random_surroundings_never_survives() {
    const NOISE: &[&str] = &[
        " ", "\n", "=", ":", "\"", ",", "/", "-", "_", ".", "a", "A", "1", "password", "token",
        "bearer ", "://", "x@", "}", "#",
    ];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut failures: Vec<String> = Vec::new();
    let mut tried = 0usize;
    for _ in 0..200_000 {
        let mut payload = String::new();
        for _ in 0..(rng.next() % 4) {
            payload.push_str(NOISE[(rng.next() as usize) % NOISE.len()]);
        }
        payload.push_str(KEY);
        for _ in 0..(rng.next() % 4) {
            payload.push_str(NOISE[(rng.next() as usize) % NOISE.len()]);
        }
        tried += 1;
        if scan(&payload).text.contains(&KEY[30..70]) {
            failures.push(payload);
            if failures.len() >= 6 {
                break;
            }
        }
    }
    println!("  tried {tried} surroundings, {} let the key through", failures.len());
    for failure in &failures {
        println!("  --- payload: {failure:?}");
        println!("      output:  {:?}", scan(failure).text);
    }
    assert!(failures.is_empty(), "{} surroundings let a key through", failures.len());
}
