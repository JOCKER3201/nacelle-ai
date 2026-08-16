//! Protocol v0, round-tripped over a pair of connected in-memory
//! streams — the serve loop under test is the one the daemon ships,
//! with no real socket anywhere and no network at all.
//!
//! The client half of these tests is written strictly to the spec page
//! (`.gap-program/decyzja-nacelle-ai-daemon.md`): it sends the command
//! lines as written there and keys on `ev` and `id`, exactly as the
//! real client fleet does.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use nacelle_ai::backend::{Backend, EventSink};
use nacelle_ai::error::BackendError;
use nacelle_ai::event::{StopReason, StreamEvent, Usage};
use nacelle_ai::message::{Request, ToolCall, ToolDeclaration};
use nacelle_ai::{Agent, Change, Effect, ToolOutput, ToolRegistry, Worker};
use nacelle_ai_daemon::backends::{Session, World};
use nacelle_ai_daemon::media::Ffmpeg;
use nacelle_ai_daemon::proto::{Command, Event, Wanted, PROTO};
use nacelle_ai_daemon::serve;
use serde_json::{json, Value};

// ---------------------------------------------------------------- pipes

/// One direction of a connection: what one side writes, the other
/// reads. Dropping the writer is the half-close the reader sees as EOF.
struct PipeWriter {
    to: Sender<Vec<u8>>,
}

struct PipeReader {
    from: Receiver<Vec<u8>>,
    rest: Vec<u8>,
}

fn pipe() -> (PipeWriter, PipeReader) {
    let (to, from) = mpsc::channel();
    (
        PipeWriter { to },
        PipeReader {
            from,
            rest: Vec::new(),
        },
    )
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.to
            .send(buf.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the other side hung up"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.rest.is_empty() {
            match self.from.recv() {
                Ok(chunk) => self.rest = chunk,
                Err(_) => return Ok(0), // EOF
            }
        }
        let n = buf.len().min(self.rest.len());
        buf[..n].copy_from_slice(&self.rest[..n]);
        self.rest.drain(..n);
        Ok(n)
    }
}

// ------------------------------------------------------- scripted world

/// A backend that answers from a script: each `send` plays the next
/// turn's events into the sink. `pace` slows the playback down so a
/// test can land a cancel mid-turn.
struct Scripted {
    turns: Vec<Vec<StreamEvent>>,
    at: usize,
    pace: Duration,
}

impl Backend for Scripted {
    fn name(&self) -> &str {
        "fake"
    }

    fn is_local(&self) -> bool {
        true
    }

    fn send(&mut self, _request: &Request, sink: &mut EventSink<'_>) -> Result<(), BackendError> {
        let turn = self.turns.get(self.at).cloned().unwrap_or_default();
        self.at += 1;
        for event in turn {
            if !self.pace.is_zero() {
                thread::sleep(self.pace);
            }
            if sink(event) == nacelle_ai::Flow::Stop {
                return Err(BackendError::Cancelled);
            }
        }
        Ok(())
    }
}

/// A registry with one tool, which may or may not need an approval.
struct OneTool {
    change: bool,
}

impl ToolRegistry for OneTool {
    fn declarations(&self) -> Vec<ToolDeclaration> {
        Vec::new()
    }

    fn effect(&self, _call: &ToolCall) -> Effect {
        if self.change {
            Effect::Change(Change::new("set the theme to amber"))
        } else {
            Effect::Read
        }
    }

    fn invoke(&mut self, _call: &ToolCall) -> ToolOutput {
        ToolOutput::ok("done")
    }
}

/// A world with a script instead of a machine.
struct TestWorld {
    script: Vec<Vec<StreamEvent>>,
    change: bool,
    pace: Duration,
}

impl TestWorld {
    fn scripted(script: Vec<Vec<StreamEvent>>) -> TestWorld {
        TestWorld {
            script,
            change: false,
            pace: Duration::ZERO,
        }
    }

    fn empty() -> TestWorld {
        TestWorld::scripted(Vec::new())
    }
}

impl World for TestWorld {
    fn backends(&mut self) -> Vec<String> {
        vec!["fake".to_string()]
    }

    fn session(&mut self, _asked: Wanted) -> Result<Session, String> {
        if self.script.is_empty() {
            return Err("no backend here".to_string());
        }
        let backend = Scripted {
            turns: std::mem::take(&mut self.script),
            at: 0,
            pace: self.pace,
        };
        let agent = Agent::new(
            Box::new(backend),
            Box::new(OneTool {
                change: self.change,
            }),
            "fake-model",
        );
        let (worker, events) = Worker::spawn(agent).map_err(|e| e.to_string())?;
        Ok(Session {
            worker,
            events,
            manifests: None,
        })
    }

    fn ffmpeg(&mut self) -> Result<Ffmpeg, String> {
        Err("no ffmpeg in this test".to_string())
    }
}

// ------------------------------------------------------------- harness

/// The client's end of a served connection.
struct Client {
    to: PipeWriter,
    from: BufReader<PipeReader>,
}

impl Client {
    fn start(world: TestWorld) -> Client {
        let (client_writes, daemon_reads) = pipe();
        let (daemon_writes, client_reads) = pipe();
        thread::spawn(move || {
            let mut world = world;
            serve::run(daemon_reads, daemon_writes, &mut world);
        });
        Client {
            to: client_writes,
            from: BufReader::new(client_reads),
        }
    }

    fn send(&mut self, line: &str) {
        writeln!(self.to, "{line}").expect("the daemon hung up");
    }

    /// The next event, whatever it is.
    fn event(&mut self) -> Value {
        let mut line = String::new();
        let n = self.from.read_line(&mut line).expect("read failed");
        assert!(n > 0, "the daemon closed the connection mid-test");
        serde_json::from_str(line.trim()).expect("the daemon wrote a line that is not JSON")
    }

    /// The next event that is not `ev:progress` — the spec lets the
    /// daemon narrate as much as it likes, and a client must not break
    /// on narration.
    fn beat(&mut self) -> Value {
        loop {
            let event = self.event();
            if event["ev"] != "progress" {
                return event;
            }
        }
    }

    /// Drain to the end of one command: deltas accumulate, progress
    /// vanishes, and the terminal `done` or `error` comes back with
    /// the text that streamed before it.
    fn finish(&mut self) -> (Value, String) {
        let mut streamed = String::new();
        loop {
            let event = self.beat();
            if event["ev"] == "delta" {
                streamed.push_str(event["text"].as_str().unwrap_or_default());
                continue;
            }
            return (event, streamed);
        }
    }
}

fn text_turn(text: &[&str]) -> Vec<StreamEvent> {
    let mut turn = vec![StreamEvent::Start {
        model: "fake-model".to_string(),
    }];
    for piece in text {
        turn.push(StreamEvent::Text(piece.to_string()));
    }
    turn.push(StreamEvent::End {
        stop: StopReason::EndTurn,
        usage: Usage::default(),
    });
    turn
}

fn tool_turn(name: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::Start {
            model: "fake-model".to_string(),
        },
        StreamEvent::ToolCall(ToolCall {
            id: "t1".to_string(),
            name: name.to_string(),
            input: json!({}),
        }),
        StreamEvent::End {
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        },
    ]
}

// --------------------------------------------------------------- tests

#[test]
fn hello_is_answered_with_hello() {
    let mut client = Client::start(TestWorld::empty());
    client.send(r#"{"cmd":"hello","client":"test","proto":0}"#);
    let hello = client.event();
    assert_eq!(hello["ev"], "hello");
    assert_eq!(hello["proto"], 0);
    assert_eq!(hello["backends"], json!(["fake"]));
}

#[test]
fn an_ask_streams_deltas_and_ends_in_done() {
    let mut client = Client::start(TestWorld::scripted(vec![text_turn(&["Hel", "lo"])]));
    client.send(r#"{"cmd":"ask","id":7,"text":"hi","backend":"local"}"#);

    let first = client.beat();
    assert_eq!(first["ev"], "delta");
    assert_eq!(first["id"], 7);
    assert_eq!(first["text"], "Hel");

    let second = client.beat();
    assert_eq!(second["ev"], "delta");
    assert_eq!(second["text"], "lo");

    let done = client.beat();
    assert_eq!(done["ev"], "done");
    assert_eq!(done["id"], 7);
    assert_eq!(done["text"], "Hello");
}

#[test]
fn a_change_waits_for_approval_and_a_no_reaches_the_model() {
    let mut world = TestWorld::scripted(vec![
        tool_turn("nacelle_set_theme"),
        text_turn(&["as you wish"]),
    ]);
    world.change = true;
    let mut client = Client::start(world);
    client.send(r#"{"cmd":"ask","id":3,"text":"make it amber","backend":"auto"}"#);

    let approval = client.beat();
    assert_eq!(approval["ev"], "approval");
    assert_eq!(approval["id"], 3);
    assert!(
        approval["desc"]
            .as_str()
            .unwrap()
            .contains("set the theme to amber"),
        "the approval names the change: {approval}"
    );

    // While the question is open, the connection is busy — a second
    // command is refused rather than queued behind an unanswered
    // question. (This is also the one deterministic moment to test it.)
    client.send(r#"{"cmd":"ask","id":9,"text":"and blue","backend":"auto"}"#);
    let busy = client.beat();
    assert_eq!(busy["ev"], "error");
    assert_eq!(busy["id"], 9);

    client.send(r#"{"cmd":"approve","id":3,"allow":false}"#);
    let (done, streamed) = client.finish();
    assert_eq!(done["ev"], "done");
    assert_eq!(done["id"], 3);
    assert_eq!(done["text"], "as you wish");
    assert_eq!(streamed, "as you wish", "the answer streamed as deltas first");
}

#[test]
fn an_approved_change_runs() {
    let mut world = TestWorld::scripted(vec![
        tool_turn("nacelle_set_theme"),
        text_turn(&["it is amber now"]),
    ]);
    world.change = true;
    let mut client = Client::start(world);
    client.send(r#"{"cmd":"ask","id":4,"text":"make it amber","backend":"auto"}"#);

    let approval = client.beat();
    assert_eq!(approval["ev"], "approval");
    client.send(r#"{"cmd":"approve","id":4,"allow":true}"#);

    let (done, _streamed) = client.finish();
    assert_eq!(done["ev"], "done");
    assert_eq!(done["id"], 4);
    assert_eq!(done["text"], "it is amber now");
    assert_eq!(done["tools_run"], 1);
}

#[test]
fn a_cancel_ends_the_ask_as_a_cancellation_not_an_error() {
    // Two hundred fragments, a millisecond apart: the cancel lands
    // somewhere in the middle of the turn.
    let long: Vec<String> = (0..200).map(|i| format!("w{i} ")).collect();
    let refs: Vec<&str> = long.iter().map(String::as_str).collect();
    let mut world = TestWorld::scripted(vec![text_turn(&refs)]);
    world.pace = Duration::from_millis(1);
    let mut client = Client::start(world);

    client.send(r#"{"cmd":"ask","id":5,"text":"talk forever","backend":"local"}"#);
    let first = client.beat();
    assert_eq!(first["ev"], "delta");
    client.send(r#"{"cmd":"cancel","id":5}"#);

    loop {
        let event = client.beat();
        if event["ev"] == "delta" {
            continue;
        }
        assert_eq!(event["ev"], "done", "a stop is not a failure: {event}");
        assert_eq!(event["id"], 5);
        assert_eq!(event["cancelled"], true);
        break;
    }
}

#[test]
fn a_backendless_machine_answers_the_ask_with_an_error() {
    let mut client = Client::start(TestWorld::empty());
    client.send(r#"{"cmd":"ask","id":11,"text":"hi","backend":"local"}"#);
    let error = client.beat();
    assert_eq!(error["ev"], "error");
    assert_eq!(error["id"], 11);
    assert_eq!(error["msg"], "no backend here");
}

#[test]
fn the_unbuilt_tools_take_the_command_and_say_so() {
    let mut client = Client::start(TestWorld::empty());
    for (id, tool) in [(21, "photo"), (22, "sort")] {
        client.send(&format!(r#"{{"cmd":"tool","id":{id},"tool":"{tool}","args":{{}}}}"#));
        let error = client.beat();
        assert_eq!(error["ev"], "error");
        assert_eq!(error["id"], id);
        assert_eq!(
            error["msg"],
            format!("the {tool} tool is not built yet"),
            "the skeleton answers, it does not improvise"
        );
    }
}

#[test]
fn a_tool_nobody_wrote_is_an_error_with_the_callers_id() {
    let mut client = Client::start(TestWorld::empty());
    client.send(r#"{"cmd":"tool","id":30,"tool":"transcode","args":{}}"#);
    let error = client.beat();
    assert_eq!(error["ev"], "error");
    assert_eq!(error["id"], 30);
    assert!(error["msg"].as_str().unwrap().contains("transcode"));
}

#[test]
fn garbage_and_half_commands_are_answered_not_ignored() {
    let mut client = Client::start(TestWorld::empty());

    client.send("this is not json");
    let error = client.event();
    assert_eq!(error["ev"], "error");
    assert_eq!(error["id"], 0);

    client.send(r#"{"cmd":"ask","id":2}"#);
    let error = client.event();
    assert_eq!(error["ev"], "error");
    assert_eq!(error["id"], 2);
    assert!(error["msg"].as_str().unwrap().contains("text"));

    client.send(r#"{"cmd":"selfdestruct","id":1}"#);
    let error = client.event();
    assert_eq!(error["ev"], "error");
    assert!(error["msg"].as_str().unwrap().contains("selfdestruct"));
}

#[test]
fn an_approve_with_nothing_waiting_is_an_error() {
    let mut client = Client::start(TestWorld::empty());
    client.send(r#"{"cmd":"approve","id":8,"allow":true}"#);
    let error = client.event();
    assert_eq!(error["ev"], "error");
    assert_eq!(error["id"], 8);
}

// ------------------------------------------------- the shapes themselves

/// Every command line the spec writes, parsed to what it means.
#[test]
fn every_spec_command_parses() {
    assert_eq!(
        Command::parse(r#"{"cmd":"hello","client":"widget","proto":0}"#).unwrap(),
        Command::Hello {
            client: "widget".to_string(),
            proto: 0
        }
    );
    assert_eq!(
        Command::parse(r#"{"cmd":"ask","id":1,"text":"hi","backend":"claude"}"#).unwrap(),
        Command::Ask {
            id: 1,
            text: "hi".to_string(),
            backend: Wanted::Claude
        }
    );
    assert_eq!(
        Command::parse(r#"{"cmd":"tool","id":2,"tool":"loop","args":{"path":"/a/b.mp4"}}"#)
            .unwrap(),
        Command::Tool {
            id: 2,
            tool: "loop".to_string(),
            args: json!({"path": "/a/b.mp4"})
        }
    );
    assert_eq!(
        Command::parse(r#"{"cmd":"approve","id":3,"allow":true}"#).unwrap(),
        Command::Approve { id: 3, allow: true }
    );
    assert_eq!(
        Command::parse(r#"{"cmd":"cancel","id":4}"#).unwrap(),
        Command::Cancel { id: 4 }
    );
}

/// Every event, written and read back: the line is one JSON object,
/// newline-terminated, carrying exactly the spec's fields.
#[test]
fn every_event_line_round_trips() {
    let cases: Vec<(Event, Value)> = vec![
        (
            Event::Hello {
                backends: vec!["local".to_string(), "claude".to_string()],
            },
            json!({"ev": "hello", "proto": PROTO, "backends": ["local", "claude"]}),
        ),
        (
            Event::Delta {
                id: 1,
                text: "hi".to_string(),
            },
            json!({"ev": "delta", "id": 1, "text": "hi"}),
        ),
        (Event::done(2), json!({"ev": "done", "id": 2})),
        (
            Event::Done {
                id: 2,
                extra: json!({"path": "/a/b-loop.mp4"}),
            },
            json!({"ev": "done", "id": 2, "path": "/a/b-loop.mp4"}),
        ),
        (
            Event::Approval {
                id: 3,
                desc: "set the theme to amber".to_string(),
            },
            json!({"ev": "approval", "id": 3, "desc": "set the theme to amber"}),
        ),
        (
            Event::Progress {
                id: 4,
                msg: "rendering".to_string(),
            },
            json!({"ev": "progress", "id": 4, "msg": "rendering"}),
        ),
        (
            Event::Error {
                id: 5,
                msg: "no".to_string(),
            },
            json!({"ev": "error", "id": 5, "msg": "no"}),
        ),
    ];
    for (event, expected) in cases {
        let line = event.line();
        assert!(line.ends_with('\n'), "a line ends the line");
        assert_eq!(line.matches('\n').count(), 1, "one object per line");
        let read: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(read, expected);
    }
}

/// `done`'s extras must never overwrite the envelope.
#[test]
fn the_done_envelope_cannot_be_overwritten() {
    let sneaky = Event::Done {
        id: 9,
        extra: json!({"ev": "hello", "id": 1, "note": "kept"}),
    };
    let read: Value = serde_json::from_str(sneaky.line().trim()).unwrap();
    assert_eq!(read["ev"], "done");
    assert_eq!(read["id"], 9);
    assert_eq!(read["note"], "kept");
}
