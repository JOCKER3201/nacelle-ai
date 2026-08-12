//! The agent on a thread of its own, talking to the interface through a
//! channel.
//!
//! [`Agent::ask`](super::Agent::ask) blocks from the first byte of the
//! request to the last byte of the reply — seconds, sometimes minutes
//! with tools. The desktop's event loop cannot afford a single frame of
//! that: a window that stops repainting while the agent thinks is a
//! window the compositor marks as hung.
//!
//! So the agent runs here instead. The interface holds a [`Worker`],
//! posts questions to it, and drains a [`std::sync::mpsc::Receiver`] on
//! whatever schedule it already has — once per frame, in a winit
//! `about_to_wait`, wherever it likes. No reactor, no executor, no
//! shared state to lock: a channel and a thread.
//!
//! Two things cross back the other way, and both are the reason this is
//! more than `thread::spawn`.
//!
//! **Stopping.** The user presses the stop button while the model is
//! mid-sentence, and it has to take effect then, not when the reply
//! finishes. [`Worker::cancel`] sets a flag that the sink reads on the
//! next event, so the turn ends at the next fragment that arrives.
//! What it cannot do is interrupt a socket that has gone quiet — a
//! provider that stops sending stops the turn on its own timeout, and
//! that is the one case where stopping is not immediate.
//!
//! **Approving.** A tool that changes something has to be put to a
//! person, and the person is on the other thread. The worker sends a
//! [`PendingApproval`] down the same channel as everything else and
//! blocks until it is answered — blocking the *worker* is exactly
//! right, since there is nothing to do until the user decides, and the
//! interface keeps drawing throughout. An approval that is dropped
//! rather than answered counts as a refusal; the model is told, and the
//! exchange carries on.

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::backend::Flow;
use crate::message::{Message, ToolCall};
use crate::supervise::seal::Stop;

use super::approval::{ApprovalRequest, Approver, Decision};
use super::registry::Change;
use super::{Agent, AgentError, AgentEvent};

/// How often a worker waiting on an approval looks up to see whether
/// the user gave up on the whole exchange instead of answering.
///
/// Polling only happens while an approval is open — that is, while a
/// dialog is on screen — so the cost is nothing, and it buys the
/// property that a stop always works even against an interface that
/// forgot to answer its own question.
const APPROVAL_POLL: Duration = Duration::from_millis(50);

/// Which question a worker is working on.
///
/// Handed out by [`Worker::ask`] so an interface that queued more than
/// one can tell the answers apart, and so a single one can be stopped
/// with [`Cancel::cancel_turn`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TurnId(pub u64);

/// The stop button.
///
/// Cheap to clone and safe to keep anywhere: a keyboard handler, a
/// button, a timeout. Stopping is by number rather than by a plain
/// flag because of one ordinary sequence — the user presses return and
/// then escape, faster than the worker got out of bed. A flag set
/// before the worker started would either be cleared and lost, or left
/// set and applied to the *next* question. A number is neither: what is
/// cancelled is every question handed out so far, whether it has begun
/// or not.
#[derive(Clone, Debug, Default)]
pub struct Cancel {
    state: Arc<CancelState>,
}

#[derive(Debug, Default)]
struct CancelState {
    issued: AtomicU64,
    cancelled_upto: AtomicU64,
}

impl Cancel {
    /// Stop whatever is running, and anything already queued.
    pub fn cancel(&self) {
        let issued = self.state.issued.load(Ordering::SeqCst);
        self.state.cancelled_upto.fetch_max(issued, Ordering::SeqCst);
    }

    /// Stop one question — and, unavoidably, anything older that is
    /// somehow still going, since older work cannot outlive a stop
    /// aimed at what came after it.
    pub fn cancel_turn(&self, turn: TurnId) {
        self.state
            .cancelled_upto
            .fetch_max(turn.0, Ordering::SeqCst);
    }

    /// Whether `turn` has been stopped. Ids count from one, so the
    /// initial zero cancels nothing.
    pub fn is_cancelled(&self, turn: TurnId) -> bool {
        self.state.cancelled_upto.load(Ordering::SeqCst) >= turn.0
    }

    fn issue(&self) -> TurnId {
        TurnId(self.state.issued.fetch_add(1, Ordering::SeqCst) + 1)
    }
}

/// A change waiting on the user.
///
/// Answer it with [`PendingApproval::allow`],
/// [`PendingApproval::deny`] or [`PendingApproval::cancel`]. Each takes
/// `self`, so a request cannot be answered twice.
///
/// **Dropping it is a refusal.** An interface that closed the window,
/// lost the dialog or simply forgot must not be able to produce a yes
/// by accident, so the absence of an answer is a no — and the model is
/// told as much, rather than being left waiting.
#[derive(Debug)]
pub struct PendingApproval {
    call: ToolCall,
    change: Change,
    reply: Sender<Decision>,
}

impl PendingApproval {
    /// The call as the model made it, arguments included.
    pub fn call(&self) -> &ToolCall {
        &self.call
    }

    /// What the registry says it would change — the sentence to show.
    pub fn change(&self) -> &Change {
        &self.change
    }

    pub fn allow(self) {
        self.answer(Decision::Allow);
    }

    pub fn deny(self, reason: impl Into<String>) {
        self.answer(Decision::deny(reason));
    }

    /// No, without saying why.
    pub fn denied(self) {
        self.answer(Decision::denied());
    }

    /// No, and stop the whole exchange.
    pub fn cancel(self) {
        self.answer(Decision::Cancel);
    }

    fn answer(self, decision: Decision) {
        // The worker having gone away is not the interface's problem
        // and not worth an error it could do nothing about.
        let _ = self.reply.send(decision);
    }
}

/// The worker has stopped, so there is nowhere to send a question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkerGone;

impl fmt::Display for WorkerGone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the agent worker has stopped")
    }
}

impl std::error::Error for WorkerGone {}

enum Command {
    Ask { turn: TurnId, message: Message },
}

/// The interface's handle on a running agent.
pub struct Worker {
    /// `None` once the worker has been shut down. Dropping the sender
    /// is what tells the thread there will be no more questions.
    commands: Option<Sender<Command>>,
    cancel: Cancel,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    /// Put `agent` on a thread and return the handle and the events.
    ///
    /// The channel is unbounded, deliberately: it carries text
    /// fragments as fast as a model produces them, and a bound would
    /// mean the worker blocking on an interface that was busy drawing
    /// — which is the deadlock this whole arrangement exists to avoid.
    /// The interface must drain it, and dropping the receiver stops the
    /// agent rather than growing a queue nobody reads.
    pub fn spawn(agent: Agent) -> io::Result<(Worker, Receiver<AgentEvent>)> {
        let (commands, queue) = mpsc::channel::<Command>();
        let (events, inbox) = mpsc::channel::<AgentEvent>();
        let cancel = Cancel::default();

        let thread = thread::Builder::new()
            .name("nacelle-ai-agent".to_string())
            .spawn({
                let cancel = cancel.clone();
                move || run(agent, queue, events, cancel)
            })?;

        Ok((
            Worker {
                commands: Some(commands),
                cancel,
                thread: Some(thread),
            },
            inbox,
        ))
    }

    /// Queue a question. Returns as soon as it is queued — the answer
    /// arrives on the receiver.
    pub fn ask(&self, question: impl Into<String>) -> Result<TurnId, WorkerGone> {
        self.ask_message(Message::user(question))
    }

    pub fn ask_message(&self, message: Message) -> Result<TurnId, WorkerGone> {
        let commands = self.commands.as_ref().ok_or(WorkerGone)?;
        // Issued before it is sent, so that a stop arriving between the
        // two still applies to this question.
        let turn = self.cancel.issue();
        commands
            .send(Command::Ask { turn, message })
            .map_err(|_| WorkerGone)?;
        Ok(turn)
    }

    /// Stop whatever is running and anything queued behind it.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// A stop button to keep somewhere else — a key handler, a widget,
    /// a watchdog.
    pub fn cancel_handle(&self) -> Cancel {
        self.cancel.clone()
    }

    /// Stop everything and wait for the thread to finish.
    ///
    /// Waits, unlike dropping, so a caller that is about to unload the
    /// library or tear down what the tools write to can be sure nothing
    /// is still running. It can take as long as the provider takes to
    /// answer the fragment that is in flight.
    pub fn shutdown(mut self) {
        self.cancel.cancel();
        self.commands.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.commands.take();
        // Deliberately not joined. Dropping happens on the interface's
        // thread, often while it is closing a window, and blocking
        // there until a network read gives up is exactly the freeze
        // this module exists to prevent. The thread sees the closed
        // queue and ends on its own; anything that must wait calls
        // [`Worker::shutdown`] and says so.
    }
}

/// The worker thread.
fn run(
    mut agent: Agent,
    queue: Receiver<Command>,
    events: Sender<AgentEvent>,
    cancel: Cancel,
) {
    // Ends when the Worker is dropped and the sender with it.
    for Command::Ask { turn, message } in queue {
        // Stopped while it was still in the queue — say so, so the
        // interface's count of outstanding questions comes back down.
        if cancel.is_cancelled(turn) {
            if events.send(AgentEvent::Failed(AgentError::Cancelled)).is_err() {
                break;
            }
            continue;
        }

        // The same question the sink answers with `Flow::Stop`, asked
        // where no event is produced for a sink to answer: the stretch
        // between handing the request over and the socket being written
        // to, which on a remote backend is layers 2, 3 and 4. Layer 3 is
        // a whole turn against a local model and layer 4 waits on a
        // person, and a stop pressed in there used to be read for the
        // first time while the REPLY was being decoded — by which point
        // the payload had gone.
        agent.stops_when(Stop::new({
            let cancel = cancel.clone();
            move || cancel.is_cancelled(turn)
        }));

        let mut approver = ChannelApprover {
            events: &events,
            cancel: &cancel,
            turn,
        };

        let result = agent.ask_message(message, &mut approver, &mut |event| {
            if cancel.is_cancelled(turn) {
                return Flow::Stop;
            }
            // Nobody is reading. Continuing would spend the user's
            // money on an answer with no destination.
            if events.send(event).is_err() {
                return Flow::Stop;
            }
            Flow::Continue
        });

        let ended = match result {
            Ok(completion) => AgentEvent::Finished(completion),
            // A turn that failed *and* was stopped is a stopped turn.
            // The door refuses a cancelled turn with a sentence of its
            // own, and an interface that showed that sentence as a
            // backend failure would be telling the user something went
            // wrong when what happened is that they pressed stop.
            Err(_) if cancel.is_cancelled(turn) => AgentEvent::Failed(AgentError::Cancelled),
            Err(err) => AgentEvent::Failed(err),
        };
        if events.send(ended).is_err() {
            break;
        }
    }
}

/// The approver that lives on the worker thread and asks the interface.
struct ChannelApprover<'a> {
    events: &'a Sender<AgentEvent>,
    cancel: &'a Cancel,
    turn: TurnId,
}

impl Approver for ChannelApprover<'_> {
    fn approve(&mut self, request: ApprovalRequest<'_>) -> Decision {
        if self.cancel.is_cancelled(self.turn) {
            return Decision::Cancel;
        }

        let (reply, answer) = mpsc::channel();
        let pending = PendingApproval {
            call: request.call.clone(),
            change: request.change.clone(),
            reply,
        };

        if self.events.send(AgentEvent::Approval(pending)).is_err() {
            // There is no interface left to ask, and an unanswerable
            // question is not a reason to proceed unasked.
            return Decision::Cancel;
        }

        loop {
            match answer.recv_timeout(APPROVAL_POLL) {
                Ok(decision) => return decision,
                Err(RecvTimeoutError::Timeout) => {
                    if self.cancel.is_cancelled(self.turn) {
                        return Decision::Cancel;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    // The request was dropped instead of answered. That
                    // is a no — see [`PendingApproval`].
                    return Decision::Deny {
                        reason: Some(
                            "the interface closed the request without an answer".to_string(),
                        ),
                    };
                }
            }
        }
    }
}
