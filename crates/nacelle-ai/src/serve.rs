//! One connection: commands in, events out, approvals held open.
//!
//! The loop is written against [`std::io::Read`] and [`std::io::Write`]
//! rather than against a socket, which is what lets the protocol tests
//! drive it over a pair of connected in-memory streams — the wire
//! behaviour under test is exactly the wire behaviour shipped.
//!
//! **Nothing without a command.** Every byte this module writes is the
//! consequence of a line the client sent. The one blocking wait in the
//! idle state is `commands.recv()`: no command, no work, no output.
//!
//! One connection runs one command at a time. The widgets each hold a
//! connection of their own, so this serialises a single widget's
//! commands and nothing else; a second `ask` or `tool` sent while one
//! is running is answered with `ev:error` rather than queued silently.
//!
//! **Approvals.** A change the agent wants to make ([`PendingApproval`])
//! and a payload about to leave the machine ([`PendingDisclosure`])
//! both travel to the client as `ev:approval` and wait for
//! `cmd:approve` — per action, every time. There is no "allow all",
//! because the protocol has no way to say it and this module does not
//! invent one. An approval the client never answers — the connection
//! closes, the widget dies — is DROPPED, and dropping either type is a
//! refusal by construction, in the core, not here.
//!
//! **How a command ends.** Every `ask` and every `tool` ends in exactly
//! one of `ev:done` or `ev:error` carrying its id. A cancellation is a
//! `done` with `"cancelled": true` in the extras — the spec leaves
//! `done`'s tail open, and a client keys on `ev` and `id`.
//!
//! **Which version, and which client.** A `hello` is answered by
//! [`Handshake`], which compares the version the client named against
//! the ones this daemon speaks and writes the client's own name on
//! stderr. Both halves used to be missing: the version was parsed and
//! dropped, and nothing anywhere said which of the four widgets a
//! connection belonged to.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::Duration;

use nacelle_ai::{AgentError, AgentEvent, PendingApproval, TurnId};
use serde_json::json;

use crate::backends::{Session, World};
use crate::media::{self, Course, Outcome};
use crate::proto::{self, Command, Event, Fault, Wanted};

/// How often the pump looks away from the worker's events to poll the
/// client's commands and layer 4's manifests. Only ticks while a
/// command is running — the idle state blocks properly.
const TICK: Duration = Duration::from_millis(25);

/// Said to an `ask` or `tool` that arrives while one is running.
const BUSY: &str =
    "another command is still running on this connection — one at a time; open a second \
     connection for parallel work";

/// Said to an `approve` that nothing is waiting for.
const NOTHING_WAITING: &str = "nothing is waiting for an approval";

/// What the connection's `hello` settled — the version negotiation, in
/// one place, because a `hello` can arrive in three: the idle loop, the
/// pump of a running ask, and the command drain of a running tool.
///
/// Until 2026-08-18 the `proto` field was read off the wire and thrown
/// away, so a client speaking a version this daemon does not know was
/// answered `{"ev":"hello","proto":0}` and went on to be served as
/// though the two agreed. Two things follow from taking it seriously:
/// the refusal names the versions there are, so a client can choose;
/// and a connection whose client SAID it speaks something else does no
/// work until it says otherwise.
///
/// What this deliberately does not do is require a handshake. A client
/// that says nothing at all is served, as it always was — v0 has no
/// required `hello`, the four widgets are written against a page that
/// does not demand one, and a rule any client can escape by staying
/// silent is not a rule. The gate is on a client that named a version,
/// which is a client that told us it will misread the answer.
#[derive(Default)]
struct Handshake {
    /// The version a client named that this daemon does not speak.
    refused: Option<u64>,
}

impl Handshake {
    /// A `hello` off the wire: the event that answers it, and the state
    /// it leaves behind. `names` is what an accepted hello reports.
    fn greet(&mut self, client: &str, proto: u64, names: &[String]) -> Event {
        let who = proto::client_label(client);
        if proto::speaks(proto) {
            self.refused = None;
            // The one line that says which of the four widgets is on the
            // other end of this connection. The daemon has no window and
            // no log of its own; stderr belongs to whoever started it.
            eprintln!("nacelle-ai: {who} connected, protocol {proto}");
            Event::Hello {
                backends: names.to_vec(),
            }
        } else {
            self.refused = Some(proto);
            eprintln!(
                "nacelle-ai: {who} asked for protocol {proto}, which this daemon does not speak"
            );
            Event::Error {
                id: 0,
                msg: proto::version_refused(&who, proto),
            }
        }
    }

    /// Why an `ask` or a `tool` cannot run, when it cannot.
    fn blocked(&self) -> Option<String> {
        self.refused.map(proto::version_pending)
    }
}

/// Serve one client until it hangs up.
///
/// `reader` and `writer` are the two halves of one connection — for the
/// daemon a [`UnixStream`](std::os::unix::net::UnixStream) and its
/// `try_clone`, for a test two in-memory pipes. Returns when the client
/// closes or the connection breaks; dropping the sessions on the way
/// out cancels whatever was still running.
pub fn run<R, W>(reader: R, mut writer: W, world: &mut dyn World)
where
    R: Read + Send + 'static,
    W: Write,
{
    let commands = read_commands(reader);
    let mut sessions: HashMap<&'static str, Session> = HashMap::new();
    // The backends list, cached at the last idle `hello` so a `hello`
    // arriving mid-command can be answered without the world.
    let mut names: Vec<String> = Vec::new();
    let mut hand = Handshake::default();

    loop {
        let next = match commands.recv() {
            Ok(next) => next,
            // The client hung up with nothing running.
            Err(_) => return,
        };
        let held = match next {
            Ok(Command::Hello { client, proto }) => {
                // Asking the world what it can answer with reaches the
                // machine — a local server, a credential — so it is done
                // for a hello that is going to be answered with one.
                if proto::speaks(proto) {
                    names = world.backends();
                }
                let answer = hand.greet(&client, proto, &names);
                say(&mut writer, &answer)
            }
            Ok(Command::Ask { id, text, backend }) => match hand.blocked() {
                Some(msg) => say(&mut writer, &Event::Error { id, msg }),
                None => ask(
                    &mut writer, &commands, world, &mut sessions, &names, &mut hand, id, text,
                    backend,
                ),
            },
            Ok(Command::Tool { id, tool, args }) => match hand.blocked() {
                Some(msg) => say(&mut writer, &Event::Error { id, msg }),
                None => tool_run(
                    &mut writer, &commands, world, &names, &mut hand, id, &tool, &args,
                ),
            },
            Ok(Command::Approve { id, .. }) => say(&mut writer, &Event::Error {
                id,
                msg: NOTHING_WAITING.to_string(),
            }),
            // Nothing is running, so there is nothing to stop. Racing a
            // cancel against a `done` is ordinary, and answering it
            // with an error would read as one.
            Ok(Command::Cancel { .. }) => Ok(()),
            Err(fault) => say(&mut writer, &Event::Error {
                id: fault.id,
                msg: fault.msg,
            }),
        };
        if held.is_err() {
            // The connection broke mid-write. Dropping the sessions
            // cancels their workers.
            return;
        }
    }
}

/// The reader half, on a thread of its own, so the serve loop can wait
/// on the worker's events and the client's commands at once. Parsing
/// happens here too: what travels is already a [`Command`] or the
/// [`Fault`] to answer with.
fn read_commands<R: Read + Send + 'static>(reader: R) -> Receiver<Result<Command, Fault>> {
    let (to, commands) = mpsc::channel();
    let _ = thread::Builder::new()
        .name("nacelle-ai-read".to_string())
        .spawn(move || {
            let mut lines = BufReader::new(reader);
            let mut line = String::new();
            loop {
                line.clear();
                match lines.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if to.send(Command::parse(trimmed)).is_err() {
                            return;
                        }
                    }
                }
            }
        });
    commands
}

/// One line out, flushed — an event is not an event until the client
/// can read it.
fn say<W: Write>(writer: &mut W, event: &Event) -> io::Result<()> {
    writer.write_all(event.line().as_bytes())?;
    writer.flush()
}

/// The session cache's key: which conversation an `ask` joins. `auto`
/// and `local` are DIFFERENT conversations even when both run on the
/// local model — the daemon's interface agent and the user's pinned
/// chat must not share a history.
fn key(wanted: Wanted) -> &'static str {
    match wanted {
        Wanted::Auto => "auto",
        Wanted::Claude => "claude",
        Wanted::Local => "local",
    }
}

/// `cmd:ask`: put the question to the right session and pump until it
/// ends.
#[allow(clippy::too_many_arguments)]
fn ask<W: Write>(
    writer: &mut W,
    commands: &Receiver<Result<Command, Fault>>,
    world: &mut dyn World,
    sessions: &mut HashMap<&'static str, Session>,
    names: &[String],
    hand: &mut Handshake,
    id: u64,
    text: String,
    backend: Wanted,
) -> io::Result<()> {
    let k = key(backend);
    if !sessions.contains_key(k) {
        match world.session(backend) {
            Ok(session) => {
                sessions.insert(k, session);
            }
            Err(msg) => return say(writer, &Event::Error { id, msg }),
        }
    }
    let session = sessions.get_mut(k).expect("inserted above");
    let turn = match session.worker.ask(text) {
        Ok(turn) => turn,
        Err(gone) => {
            // The worker died between asks. Forget it; the next ask
            // builds a fresh one.
            let msg = gone.to_string();
            sessions.remove(k);
            return say(writer, &Event::Error { id, msg });
        }
    };
    pump(writer, commands, session, names, hand, id, turn)
}

/// What an open `ev:approval` is waiting to hand back.
enum Waiting {
    /// A tool change ([`Approver`](nacelle_ai::Approver) path).
    Approval(PendingApproval),
    /// A manifest — bytes about to leave the machine (layer 4).
    Disclosure(nacelle_ai::PendingDisclosure),
}

impl Waiting {
    fn answer(self, allow: bool) {
        match (self, allow) {
            (Waiting::Approval(p), true) => p.allow(),
            (Waiting::Approval(p), false) => p.denied(),
            (Waiting::Disclosure(d), true) => d.send(),
            (Waiting::Disclosure(d), false) => d.refused(),
        }
    }

    /// The turn is being stopped: a waiting change is cancelled, a
    /// waiting manifest is refused. Either way nothing proceeds.
    fn stop(self) {
        match self {
            Waiting::Approval(p) => p.cancel(),
            Waiting::Disclosure(d) => d.refused(),
        }
    }
}

/// The sentence an approval shows: what the registry said would change.
fn describe(pending: &PendingApproval) -> String {
    let change = pending.change();
    match &change.detail {
        Some(detail) => format!("{} — {detail}", change.summary),
        None => change.summary.clone(),
    }
}

/// Drive one `ask` to its end: worker events out as deltas and
/// progress, approvals held open, commands answered on the way.
#[allow(clippy::too_many_arguments)]
fn pump<W: Write>(
    writer: &mut W,
    commands: &Receiver<Result<Command, Fault>>,
    session: &mut Session,
    names: &[String],
    hand: &mut Handshake,
    id: u64,
    turn: TurnId,
) -> io::Result<()> {
    let cancel = session.worker.cancel_handle();
    let mut waiting: Option<Waiting> = None;

    loop {
        match session.events.recv_timeout(TICK) {
            Ok(event) => match event {
                AgentEvent::TurnStarted { turn, model } => say(writer, &Event::Progress {
                    id,
                    msg: format!("turn {turn}: {model} is answering"),
                })?,
                AgentEvent::Text(text) => say(writer, &Event::Delta { id, text })?,
                // Reasoning is the model's own; the protocol carries
                // the answer.
                AgentEvent::Thinking(_) => {}
                AgentEvent::ToolStarted { call } => say(writer, &Event::Progress {
                    id,
                    msg: format!("running {}", call.name),
                })?,
                AgentEvent::ToolFinished { name, is_error, .. } => say(writer, &Event::Progress {
                    id,
                    msg: if is_error {
                        format!("{name} failed")
                    } else {
                        format!("{name} finished")
                    },
                })?,
                AgentEvent::ToolDenied { name, reason, .. } => say(writer, &Event::Progress {
                    id,
                    msg: format!("{name} was refused: {reason}"),
                })?,
                AgentEvent::Approval(pending) => {
                    let desc = describe(&pending);
                    waiting = Some(Waiting::Approval(pending));
                    say(writer, &Event::Approval { id, desc })?;
                }
                AgentEvent::Finished(done) => {
                    return say(writer, &Event::Done {
                        id,
                        extra: json!({
                            "text": done.text,
                            "turns": done.turns,
                            "tools_run": done.tools_run,
                        }),
                    });
                }
                AgentEvent::Failed(AgentError::Cancelled) => {
                    return say(writer, &Event::Done {
                        id,
                        extra: json!({ "cancelled": true }),
                    });
                }
                AgentEvent::Failed(err) => {
                    return say(writer, &Event::Error {
                        id,
                        msg: err.to_string(),
                    });
                }
            },
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return say(writer, &Event::Error {
                    id,
                    msg: "the agent worker has stopped".to_string(),
                });
            }
        }

        // Layer 4's manifests, for a session that has them.
        if let Some(manifests) = &session.manifests {
            while let Ok(pending) = manifests.try_recv() {
                let desc = pending.manifest().render();
                waiting = Some(Waiting::Disclosure(pending));
                say(writer, &Event::Approval { id, desc })?;
            }
        }

        // The client's commands, without blocking the pump.
        loop {
            match commands.try_recv() {
                Ok(Ok(Command::Approve { id: to, allow })) => {
                    match waiting.take() {
                        Some(pending) if to == id => pending.answer(allow),
                        Some(pending) => {
                            // Wrong id: the question stays open. An
                            // approval must never be answered by a
                            // command aimed at something else.
                            waiting = Some(pending);
                            say(writer, &Event::Error {
                                id: to,
                                msg: NOTHING_WAITING.to_string(),
                            })?;
                        }
                        None => say(writer, &Event::Error {
                            id: to,
                            msg: NOTHING_WAITING.to_string(),
                        })?,
                    }
                }
                Ok(Ok(Command::Cancel { id: to })) if to == id => {
                    cancel.cancel_turn(turn);
                    if let Some(pending) = waiting.take() {
                        pending.stop();
                    }
                }
                Ok(Ok(Command::Cancel { .. })) => {}
                Ok(Ok(Command::Hello { client, proto })) => {
                    // The cached names, because asking the world again
                    // mid-turn would reach the machine while a turn is
                    // running. The negotiation itself is the same one.
                    let answer = hand.greet(&client, proto, names);
                    say(writer, &answer)?;
                }
                Ok(Ok(Command::Ask { id: to, .. })) | Ok(Ok(Command::Tool { id: to, .. })) => {
                    say(writer, &Event::Error {
                        id: to,
                        msg: BUSY.to_string(),
                    })?;
                }
                Ok(Err(fault)) => say(writer, &Event::Error {
                    id: fault.id,
                    msg: fault.msg,
                })?,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // The client hung up mid-ask. The abandoned
                    // question is a refusal, the turn is stopped, and
                    // there is nobody left to write to: returning drops
                    // the sessions at the caller, which cancels the
                    // worker for good.
                    cancel.cancel_turn(turn);
                    if let Some(pending) = waiting.take() {
                        pending.stop();
                    }
                    return Ok(());
                }
            }
        }
    }
}

/// `cmd:tool`: the deterministic tools. No model is involved on this
/// path — that is the policy, not an accident; see `backends`.
#[allow(clippy::too_many_arguments)]
fn tool_run<W: Write>(
    writer: &mut W,
    commands: &Receiver<Result<Command, Fault>>,
    world: &mut dyn World,
    names: &[String],
    hand: &mut Handshake,
    id: u64,
    tool: &str,
    args: &serde_json::Value,
) -> io::Result<()> {
    match tool {
        "loop" => {
            let ffmpeg = match world.ffmpeg() {
                Ok(ffmpeg) => ffmpeg,
                Err(msg) => return say(writer, &Event::Error { id, msg }),
            };
            let mut course = ConnCourse {
                writer,
                commands,
                names,
                hand,
                id,
                stopped: false,
                held: Ok(()),
            };
            let outcome = media::run_loop(&ffmpeg, args, &mut course);
            let ConnCourse { held, .. } = course;
            held?;
            match outcome {
                Ok(Outcome::Done(path)) => say(writer, &Event::Done {
                    id,
                    extra: json!({ "path": path.display().to_string() }),
                }),
                Ok(Outcome::Cancelled) => say(writer, &Event::Done {
                    id,
                    extra: json!({ "cancelled": true }),
                }),
                Err(msg) => say(writer, &Event::Error { id, msg }),
            }
        }
        // Named in the protocol, not built yet — the skeleton takes the
        // command and says so, which is all it may honestly do.
        "photo" | "sort" => say(writer, &Event::Error {
            id,
            msg: format!("the {tool} tool is not built yet"),
        }),
        other => say(writer, &Event::Error {
            id,
            msg: format!("there is no tool called \"{other}\" — it is loop, photo or sort"),
        }),
    }
}

/// The [`Course`] a running tool steers by: progress lines go out as
/// `ev:progress`, and "has the user said stop" is answered by draining
/// the command channel — which also keeps the connection honest about
/// commands that arrive mid-run.
struct ConnCourse<'a, W: Write> {
    writer: &'a mut W,
    commands: &'a Receiver<Result<Command, Fault>>,
    names: &'a [String],
    hand: &'a mut Handshake,
    id: u64,
    stopped: bool,
    /// The first write error, kept so the caller can stop serving a
    /// connection that is gone. A tool mid-run is not interrupted by
    /// it; it stops at the next step boundary via [`Course::stopped`].
    held: io::Result<()>,
}

impl<W: Write> ConnCourse<'_, W> {
    fn tell(&mut self, event: &Event) {
        if self.held.is_ok() {
            self.held = say(self.writer, event);
            if self.held.is_err() {
                // Nobody is listening; treat it as a stop so the run
                // ends at the next boundary instead of rendering for
                // no one.
                self.stopped = true;
            }
        }
    }
}

impl<W: Write> Course for ConnCourse<'_, W> {
    fn progress(&mut self, msg: &str) {
        self.tell(&Event::Progress {
            id: self.id,
            msg: msg.to_string(),
        });
    }

    fn stopped(&mut self) -> bool {
        loop {
            match self.commands.try_recv() {
                Ok(Ok(Command::Cancel { id })) if id == self.id => self.stopped = true,
                Ok(Ok(Command::Cancel { .. })) => {}
                Ok(Ok(Command::Hello { client, proto })) => {
                    let answer = self.hand.greet(&client, proto, self.names);
                    self.tell(&answer);
                }
                Ok(Ok(Command::Approve { id, .. })) => {
                    let error = Event::Error {
                        id,
                        msg: NOTHING_WAITING.to_string(),
                    };
                    self.tell(&error);
                }
                Ok(Ok(Command::Ask { id, .. })) | Ok(Ok(Command::Tool { id, .. })) => {
                    let error = Event::Error {
                        id,
                        msg: BUSY.to_string(),
                    };
                    self.tell(&error);
                }
                Ok(Err(fault)) => {
                    let error = Event::Error {
                        id: fault.id,
                        msg: fault.msg,
                    };
                    self.tell(&error);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.stopped = true;
                    break;
                }
            }
        }
        self.stopped
    }
}
