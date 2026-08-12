//! Watching the machine without burning it.
//!
//! "The agent monitors the whole system" cannot mean a 30B model in a
//! spin loop. Continuous inference makes the desktop unusable, costs
//! every watt the machine has, and tells nobody anything new: almost
//! every second of a running desktop is identical to the one before it.
//!
//! So this is **event-driven**. Cheap deterministic checks run
//! continuously — a counter read, a threshold compared, a widget's own
//! report picked off a queue — and they are not the model's job. When
//! one of them fires, an [`Observation`] goes down a channel and the
//! owner decides whether this is a moment that needs interpreting.
//! Nothing here ever calls a model. That is not an omission: it is what
//! makes "the model sleeps unless something happened" a property of the
//! code rather than a promise in a comment.
//!
//! Two things it owes the person whose files it can read:
//!
//! **It stops.** [`WatchHandle::pause`] and [`WatchHandle::stop`] take
//! effect within a tick, not at the end of one, because the handle is
//! read between checks as well as between rounds.
//!
//! **It says it is running.** [`Watch::describe`] and
//! [`WatchHandle::status`] exist so an interface can show it, and there
//! is no way to start one of these without a handle to ask. A background
//! process that reads the user's files and can be neither seen nor
//! stopped is not a feature.

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How finely the sleep between rounds is chopped. A stop has to be felt
/// by a person, and a person feels a fiftieth of a second as "at once"
/// while the machine feels it as an eternity of doing nothing.
const SLICE: Duration = Duration::from_millis(20);

/// The most one check may report in a single round. A check that has
/// more than this to say is broken, and the loop must not be held open
/// by it.
const MAX_PER_ROUND: usize = 16;

/// Something a check noticed.
///
/// Deliberately plain text: this is what the local model would be given
/// if the owner decides to wake one, and a structured event would only
/// have to be turned into a sentence anyway.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    /// Which check produced it.
    pub check: String,
    /// One line: what happened.
    pub summary: String,
    /// The numbers, when there are any worth passing on.
    pub detail: Option<String>,
}

impl Observation {
    pub fn new(check: impl Into<String>, summary: impl Into<String>) -> Self {
        Observation {
            check: check.into(),
            summary: summary.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

impl fmt::Display for Observation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.check, self.summary)?;
        match &self.detail {
            Some(detail) => write!(f, " ({detail})"),
            None => Ok(()),
        }
    }
}

/// One cheap, deterministic thing that runs continuously.
///
/// `look` is called once a round and must be cheap enough to run
/// forever: read a counter, compare a number, take something off a
/// queue. It returns `None` almost always, and that is the normal case —
/// a check that reports every round is a check that has been written as
/// a log.
pub trait Check: Send {
    fn name(&self) -> &str;

    /// What is worth waking somebody for, if anything.
    fn look(&mut self) -> Option<Observation>;
}

/// A number that must stay under a limit.
///
/// Fires on the CROSSING and not while the value stays over: a
/// supervisor that reported a full disk every round would wake the model
/// every round, which is the failure this whole module is shaped to
/// avoid. It re-arms when the value comes back under, so the second time
/// it happens is reported too.
pub struct Threshold {
    name: String,
    what: String,
    limit: u64,
    over: bool,
    sample: Box<dyn FnMut() -> u64 + Send>,
}

impl Threshold {
    pub fn new(
        name: impl Into<String>,
        what: impl Into<String>,
        limit: u64,
        sample: impl FnMut() -> u64 + Send + 'static,
    ) -> Self {
        Threshold {
            name: name.into(),
            what: what.into(),
            limit,
            over: false,
            sample: Box::new(sample),
        }
    }
}

impl Check for Threshold {
    fn name(&self) -> &str {
        &self.name
    }

    fn look(&mut self) -> Option<Observation> {
        let value = (self.sample)();
        let over = value > self.limit;
        let crossed = over && !self.over;
        self.over = over;
        crossed.then(|| {
            Observation::new(
                self.name.clone(),
                format!("{} went over {}", self.what, self.limit),
            )
            .with_detail(format!("{value}"))
        })
    }
}

/// A queue anything on the machine can drop an observation into.
///
/// This is how a widget reports its own anomaly: it holds a
/// [`Reporter`], which is cheap to clone and safe to keep anywhere, and
/// the supervisor picks the report up on its next round. The widget does
/// not have to know a supervisor exists, and the supervisor does not
/// have to poll the widget.
pub struct Reports {
    name: String,
    inbox: Receiver<Observation>,
}

/// The other end of a [`Reports`] queue.
#[derive(Clone, Debug)]
pub struct Reporter {
    outbox: Sender<Observation>,
}

impl Reports {
    /// A queue and the handle that fills it.
    pub fn new(name: impl Into<String>) -> (Reports, Reporter) {
        let (outbox, inbox) = mpsc::channel();
        (
            Reports {
                name: name.into(),
                inbox,
            },
            Reporter { outbox },
        )
    }
}

impl Reporter {
    /// Report something. Silently does nothing once the supervisor has
    /// stopped — a widget that outlives the watcher is not a fault, and
    /// there is nothing it could usefully do about it.
    pub fn report(&self, observation: Observation) {
        let _ = self.outbox.send(observation);
    }
}

impl Check for Reports {
    fn name(&self) -> &str {
        &self.name
    }

    /// An empty queue and a queue whose last reporter has gone are the
    /// same answer here: nothing to report. A widget that unloaded is
    /// not an event, and it is certainly not a reason to stop watching
    /// the others.
    fn look(&mut self) -> Option<Observation> {
        self.inbox.try_recv().ok()
    }
}

/// What the supervisor is doing, in the words an interface shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Running,
    /// Checks are not being run. The thread is still there and
    /// [`WatchHandle::resume`] starts it again.
    Paused,
    Stopped,
}

impl Status {
    fn code(self) -> u8 {
        match self {
            Status::Running => 0,
            Status::Paused => 1,
            Status::Stopped => 2,
        }
    }

    fn from_code(code: u8) -> Status {
        match code {
            0 => Status::Running,
            1 => Status::Paused,
            _ => Status::Stopped,
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Status::Running => "running",
            Status::Paused => "paused",
            Status::Stopped => "stopped",
        })
    }
}

/// The stop button, and the only way to ask what the supervisor is
/// doing.
///
/// Clone it into a menu item, a key handler, a widget. Once it is
/// stopped it stays stopped: a supervisor that could be restarted
/// through the same handle would be one a stray call could bring back
/// after the user turned it off.
#[derive(Clone, Debug)]
pub struct WatchHandle {
    state: Arc<AtomicU8>,
}

impl WatchHandle {
    pub fn status(&self) -> Status {
        Status::from_code(self.state.load(Ordering::SeqCst))
    }

    pub fn is_running(&self) -> bool {
        self.status() == Status::Running
    }

    /// Stop running checks, keep the thread.
    pub fn pause(&self) {
        let _ = self.state.compare_exchange(
            Status::Running.code(),
            Status::Paused.code(),
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    pub fn resume(&self) {
        let _ = self.state.compare_exchange(
            Status::Paused.code(),
            Status::Running.code(),
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
    }

    /// Stop for good.
    pub fn stop(&self) {
        self.state.store(Status::Stopped.code(), Ordering::SeqCst);
    }
}

/// The supervisor: a thread, some checks, and a channel of what they
/// noticed.
pub struct Watch {
    handle: WatchHandle,
    names: Vec<String>,
    every: Duration,
    thread: Option<JoinHandle<()>>,
}

impl Watch {
    /// Start watching, and hand back what the checks notice.
    ///
    /// `every` is how often the cheap checks run — not how often
    /// anything is interpreted. Nothing on the far end of this channel is
    /// obliged to wake a model; the channel simply has nothing in it
    /// when nothing happened, which is most of the time.
    pub fn start(
        checks: Vec<Box<dyn Check>>,
        every: Duration,
    ) -> io::Result<(Watch, Receiver<Observation>)> {
        let names = checks.iter().map(|c| c.name().to_string()).collect();
        let state = Arc::new(AtomicU8::new(Status::Running.code()));
        let (events, inbox) = mpsc::channel();

        let thread = thread::Builder::new()
            .name("nacelle-ai-supervisor".to_string())
            .spawn({
                let state = Arc::clone(&state);
                move || run(checks, events, state, every)
            })?;

        Ok((
            Watch {
                handle: WatchHandle { state },
                names,
                every,
                thread: Some(thread),
            },
            inbox,
        ))
    }

    /// A handle to keep somewhere else.
    pub fn handle(&self) -> WatchHandle {
        self.handle.clone()
    }

    pub fn status(&self) -> Status {
        self.handle.status()
    }

    pub fn pause(&self) {
        self.handle.pause();
    }

    pub fn resume(&self) {
        self.handle.resume();
    }

    /// The names of the checks that are running.
    pub fn checks(&self) -> &[String] {
        &self.names
    }

    /// One line for the interface. This is the "it must say plainly when
    /// it is running" half of the contract, and it is a method rather
    /// than something the interface composes so that every interface
    /// says the same thing.
    pub fn describe(&self) -> String {
        format!(
            "supervisor {}: {} check(s) every {}ms — {}",
            self.status(),
            self.names.len(),
            self.every.as_millis(),
            match self.names.is_empty() {
                true => "nothing to watch".to_string(),
                false => self.names.join(", "),
            }
        )
    }

    /// Stop and wait for the thread.
    ///
    /// Waiting is bounded by one slice of the sleep, so this is safe to
    /// call from an interface that is closing a window.
    pub fn stop(mut self) {
        self.handle.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        // A dropped supervisor is a stopped one, whether or not anybody
        // called stop. The thread holds the checks, and a check that
        // outlived the thing that was supposed to own it would keep
        // reading the user's machine with nothing left to report to.
        self.handle.stop();
    }
}

/// The supervisor thread.
fn run(
    mut checks: Vec<Box<dyn Check>>,
    events: Sender<Observation>,
    state: Arc<AtomicU8>,
    every: Duration,
) {
    loop {
        match Status::from_code(state.load(Ordering::SeqCst)) {
            Status::Stopped => return,
            Status::Paused => {
                // Not a round: paused means the checks do not run, not
                // that they run and are ignored.
                thread::sleep(SLICE);
                continue;
            }
            Status::Running => {}
        }

        for check in checks.iter_mut() {
            for _ in 0..MAX_PER_ROUND {
                let Some(observation) = check.look() else {
                    break;
                };
                // Nobody is listening any more. Carrying on would be a
                // thread reading the machine for an audience that has
                // gone.
                if events.send(observation).is_err() {
                    state.store(Status::Stopped.code(), Ordering::SeqCst);
                    return;
                }
            }
            if Status::from_code(state.load(Ordering::SeqCst)) == Status::Stopped {
                return;
            }
        }

        if !nap(&state, every) {
            return;
        }
    }
}

/// Sleep between rounds, in slices, and say whether to carry on.
fn nap(state: &AtomicU8, every: Duration) -> bool {
    let until = Instant::now() + every;
    loop {
        if Status::from_code(state.load(Ordering::SeqCst)) == Status::Stopped {
            return false;
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        thread::sleep(left.min(SLICE));
    }
}
