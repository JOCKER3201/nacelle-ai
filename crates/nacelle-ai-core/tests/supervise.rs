//! Escalation, and the background watch.
//!
//! Nothing here opens a socket. That is not an accident of how the tests
//! were written — it is the property being tested. The local half of
//! this agent has to work identically on a machine with no token and no
//! network, so every case below is either about deciding NOT to reach
//! for the network or about a watcher that never talks to a model at
//! all.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use nacelle_ai::redact::{Disclosure, Gathering, Manifest, Outgoing, Why};
use nacelle_ai::supervise::escalate::Decision;
use nacelle_ai::supervise::handoff::Sending;
use nacelle_ai::supervise::watch::{Check, Observation, Reports, Status, Threshold, Watch};
use nacelle_ai::{
    Attempts, BackendError, Consent, Discloser, Grounds, Handoff, NobodyToAsk, Policy, Remote,
    Trigger,
};

/// Short enough that a test is not a pause, long enough that a round is
/// a round.
const EVERY: Duration = Duration::from_millis(10);
/// How long a test waits for something that should happen at once.
const SOON: Duration = Duration::from_secs(5);

// ---- when the local agent may ask ------------------------------------

/// The user's own request is a reason on its own, and it is never
/// weighed against anything: no heuristic in here gets to decide they
/// did not mean it.
#[test]
fn the_user_asking_is_always_a_reason() {
    let policy = Policy::new(Remote::Ready);
    assert!(policy.decide(Trigger::UserAsked).is_ask());
}

/// The deterministic triggers, which exist because the local model is an
/// unreliable narrator of its own competence in both directions.
#[test]
fn the_triggers_that_do_not_depend_on_the_model_s_opinion() {
    // Twice on the same task, not once: one failure is a mistake.
    let mut attempts = Attempts::new();
    assert_eq!(attempts.failed("read the layaut"), None);
    let trigger = attempts
        .failed("read the layaut")
        .expect("the second failure escalates");
    assert_eq!(
        trigger,
        Trigger::RepeatedFailure {
            task: "read the layaut".to_string(),
            attempts: 2
        }
    );

    // A different task keeps its own count.
    assert_eq!(attempts.failed("set the theme"), None);

    // And a success clears it, so a task that stumbles months apart does
    // not escalate on the strength of ancient history.
    attempts.succeeded("read the layaut");
    assert_eq!(attempts.count("read the layaut"), 0);
    assert_eq!(attempts.failed("read the layaut"), None);

    // Arithmetic, not judgement.
    assert_eq!(Trigger::context_exceeded(1_000, 8_000), None);
    assert!(matches!(
        Trigger::context_exceeded(40_000, 8_000),
        Some(Trigger::ContextExceeded { .. })
    ));
}

/// The model may ask — but only with a reason the user can read. "I
/// think we should escalate" is not something anybody can evaluate, and
/// an escalation nobody evaluates is one that gets approved by reflex.
#[test]
fn the_model_may_ask_only_with_a_reason() {
    assert_eq!(Trigger::model_asked(""), None);
    assert_eq!(Trigger::model_asked("   \n "), None);

    let trigger = Trigger::model_asked("this needs a tool I do not have").expect("a reason");
    assert!(trigger.reason().contains("this needs a tool I do not have"));
}

/// A pinned session says what it cannot do instead of reaching for the
/// network.
#[test]
fn a_pinned_session_stays_local_and_says_so() {
    let mut policy = Policy::new(Remote::Ready);
    policy.pin();
    assert!(policy.is_pinned());
    assert!(policy.status().contains("local model only"));

    let decision = policy.decide(Trigger::MissingCapability {
        needed: "vision".to_string(),
    });
    let Decision::Stay { grounds, tell, .. } = decision else {
        panic!("a pinned session must not escalate");
    };
    assert_eq!(grounds, Grounds::PinnedLocal);
    assert!(tell.contains("I have no vision"), "{tell}");
    assert!(tell.contains("Unpin the session"), "{tell}");

    // And the pin is the user's to lift.
    policy.unpin();
    assert!(policy.decide(Trigger::UserAsked).is_ask());
}

/// The explicit request and the pin can contradict each other. The pin
/// wins and the agent says how to lift it — a request that silently
/// overrode the pin would make the pin a suggestion.
#[test]
fn an_explicit_request_does_not_quietly_unpin_the_session() {
    let policy = Policy::local_only();
    let Decision::Stay { grounds, tell, .. } = policy.decide(Trigger::UserAsked) else {
        panic!("the pin must hold");
    };
    assert_eq!(grounds, Grounds::PinnedLocal);
    assert!(tell.contains("Unpin the session"), "{tell}");
}

/// The property the whole arrangement rests on: the local half never
/// depends on the remote half being reachable. A pin, a missing token
/// and a dead network are one code path with three explanations, and the
/// sentence about what could not be done here is identical in all three.
#[test]
fn no_token_and_no_network_degrade_exactly_like_a_pin() {
    let trigger = Trigger::RepeatedFailure {
        task: "summarise the layaut".to_string(),
        attempts: 2,
    };

    let mut policies = vec![Policy::local_only()];
    policies.push(Policy::new(Remote::NoCredential(
        "no credential: set ANTHROPIC_AUTH_TOKEN".to_string(),
    )));
    policies.push(Policy::new(Remote::Unreachable(
        "network error: could not reach api.anthropic.com".to_string(),
    )));

    let mut said = Vec::new();
    let mut grounds = Vec::new();
    for policy in &policies {
        match policy.decide(trigger.clone()) {
            Decision::Ask { .. } => panic!("none of these may escalate"),
            Decision::Stay {
                grounds: why,
                tell,
                trigger: kept,
            } => {
                assert_eq!(kept, trigger, "the trigger must survive the refusal");
                said.push(tell);
                grounds.push(why);
            }
        }
    }

    let shortfall = "I could not get \"summarise the layaut\" right on my own. \
                     I am not going to ask Claude about this.";
    for tell in &said {
        assert!(
            tell.starts_with(shortfall),
            "every refusal must open by saying what could not be done here: {tell}"
        );
    }
    // Same shape, different explanation — and each explanation names
    // something the user can act on.
    assert_eq!(
        grounds,
        vec![
            Grounds::PinnedLocal,
            Grounds::NoCredential,
            Grounds::Unreachable
        ]
    );
    assert!(said[1].contains("ANTHROPIC_AUTH_TOKEN"), "{}", said[1]);
    assert!(said[2].contains("Check the network"), "{}", said[2]);
}

/// The degradation has to be something the code falls into, not
/// something a caller remembers to arrange. A turn that failed on the
/// network leaves the policy in the same place a pin does, and one that
/// failed for a reason the provider chose leaves it able to ask again —
/// a session must not pin itself over one rate limit.
#[test]
fn a_failed_turn_teaches_the_policy_what_the_network_is_doing() {
    let mut policy = Policy::new(Remote::Ready);

    policy.observe(&BackendError::RateLimited {
        retry_after: None,
        message: "slow down".to_string(),
    });
    assert!(
        policy.decide(Trigger::UserAsked).is_ask(),
        "an answer from the provider is proof it is reachable"
    );

    policy.observe(&BackendError::Network(
        "could not reach api.anthropic.com".to_string(),
    ));
    assert_eq!(policy.blocked(), Some(Grounds::Unreachable));

    // Coming back is an act, not a timeout.
    policy.set_remote(Remote::Ready);
    assert_eq!(policy.blocked(), None);

    // A credential the provider rejected is not a credential this
    // machine has: same ground, same remedy.
    policy.observe(&BackendError::Auth("invalid x-api-key".to_string()));
    assert_eq!(policy.blocked(), Some(Grounds::NoCredential));
}

// ---- the road across, and the user standing on it ---------------------

/// A discloser that records what it was shown and answers as the test
/// tells it to. It stands in for the window: what matters is that it is
/// CALLED, with a manifest that says what would leave.
struct Asked {
    answer: Consent,
    seen: Vec<String>,
}

impl Asked {
    fn saying(answer: Consent) -> Self {
        Asked {
            answer,
            seen: Vec::new(),
        }
    }
}

impl Discloser for Asked {
    fn disclose(&mut self, manifest: &Manifest) -> Consent {
        self.seen.push(manifest.render());
        self.answer.clone()
    }
}

/// A payload with a file in it and a secret in the file, so every part
/// of the manifest has something to say.
fn gathered() -> Gathering {
    Gathering::new()
        .with_text("what does this configuration do?")
        .with_file(
            "/home/michael/.config/nacelle-desktop/nacelle-desktop.conf",
            "Theme = crimson\nAPI_TOKEN = EXAMPLE-NOT-A-REAL-TOKEN-0000\n",
        )
}

fn payload() -> Outgoing {
    gathered().unreviewed()
}

/// Layer 4, as the thing that actually stands between the payload and
/// the wire. The manifest is put in front of the user by the code rather
/// than fetched by the interface, and until they answer there is no
/// payload to send — `Cleared` has no other constructor.
#[test]
fn nothing_leaves_until_the_user_has_seen_a_manifest_and_agreed() {
    let policy = Policy::new(Remote::Ready);

    // Said no.
    let mut disclosure = Disclosure::new();
    let mut refusing = Asked::saying(Consent::refuse("not that file"));
    let sending = Handoff::new(&policy, Trigger::UserAsked, "anthropic", payload())
        .clear(&mut disclosure, &mut refusing);

    assert_eq!(refusing.seen.len(), 1, "the user must have been asked");
    assert!(!sending.is_cleared());
    assert!(sending.cleared().is_none(), "there is no payload to send");
    let tell = sending.tell().expect("a decline is said out loud");
    assert!(tell.contains("nothing left this machine"), "{tell}");
    assert!(tell.contains("not that file"), "{tell}");
    assert!(
        !disclosure.has_shown(),
        "a manifest that was declined is not a disclosure that happened"
    );

    // Said yes.
    let mut agreeing = Asked::saying(Consent::Send);
    let sending = Handoff::new(&policy, Trigger::UserAsked, "anthropic", payload())
        .clear(&mut disclosure, &mut agreeing);
    let cleared = sending.cleared().expect("agreed, so it may go");
    assert_eq!(cleared.destination(), "anthropic");
    assert!(cleared.payload().contains("what does this configuration do?"));

    // What the user was shown named the file, the size and what went —
    // and never the value itself.
    let shown = &agreeing.seen[0];
    assert!(shown.contains("nacelle-desktop.conf"), "{shown}");
    assert!(shown.contains("bytes"), "{shown}");
    assert!(
        !shown.contains("EXAMPLE-NOT-A-REAL-TOKEN-0000"),
        "the manifest must not be a second copy of the payload: {shown}"
    );
    // And the payload does not carry it either — layer 2 ran first.
    assert!(!cleared.payload().contains("EXAMPLE-NOT-A-REAL-TOKEN-0000"));
}

/// A refusal by the policy stops before the user is troubled at all.
/// Asking somebody to authorise something that was never going to happen
/// is how a manifest becomes a dialog people dismiss.
#[test]
fn a_pinned_session_does_not_even_show_the_manifest() {
    let policy = Policy::local_only();

    let mut disclosure = Disclosure::new();
    let mut discloser = Asked::saying(Consent::Send);
    let sending = Handoff::new(&policy, Trigger::UserAsked, "anthropic", payload())
        .clear(&mut disclosure, &mut discloser);

    assert!(discloser.seen.is_empty(), "nobody should have been asked");
    let Sending::Refused { grounds, tell } = sending else {
        panic!("a pinned session must not clear a payload");
    };
    assert_eq!(grounds, Grounds::PinnedLocal);
    assert!(tell.contains("Unpin the session"), "{tell}");
}

/// Shown when there is something new, and not otherwise. A manifest
/// before every escalation is one that gets clicked through, which is
/// worse than none — it trains the reflex it exists to interrupt.
#[test]
fn the_manifest_returns_only_when_the_payload_carries_something_unseen() {
    let policy = Policy::new(Remote::Ready);
    let mut disclosure = Disclosure::new();

    let mut first = Asked::saying(Consent::Send);
    Handoff::new(&policy, Trigger::UserAsked, "anthropic", payload())
        .clear(&mut disclosure, &mut first);
    assert_eq!(first.seen.len(), 1, "the first escalation always asks");

    // The same file again: nothing the user has not already answered
    // for.
    let mut again = Asked::saying(Consent::Send);
    let sending = Handoff::new(&policy, Trigger::UserAsked, "anthropic", payload())
        .clear(&mut disclosure, &mut again);
    assert!(again.seen.is_empty(), "there was nothing new to disclose");
    assert!(sending.is_cleared());
    assert!(
        sending.cleared().and_then(|c| c.manifest()).is_none(),
        "no manifest was shown, and it must not claim one was"
    );

    // A file the user has not seen brings the manifest straight back.
    let mut third = Asked::saying(Consent::Send);
    let unseen = gathered()
        .with_file("/home/michael/notes/plan.txt", "the unannounced thing\n")
        .unreviewed();
    Handoff::new(&policy, Trigger::UserAsked, "anthropic", unseen)
        .clear(&mut disclosure, &mut third);
    assert_eq!(third.seen.len(), 1, "an unseen file must be disclosed");
    assert!(third.seen[0].contains("plan.txt"), "{}", third.seen[0]);
}

/// With nobody at the machine there is nobody to authorise anything, and
/// the answer is no. The stock discloser is the refusing one for the
/// same reason the stock approver is `DenyAll`.
#[test]
fn with_nobody_to_ask_the_answer_is_no() {
    let policy = Policy::new(Remote::Ready);
    let mut disclosure = Disclosure::new();
    let sending = Handoff::new(&policy, Trigger::UserAsked, "anthropic", payload())
        .clear(&mut disclosure, &mut NobodyToAsk);

    assert!(!sending.is_cleared());
    let tell = sending.tell().expect("it says why");
    assert!(tell.contains("nobody at this machine"), "{tell}");
}

/// Layer 4 across a thread boundary, which is where it actually lives:
/// the seal is inside a backend on the worker thread and the person is
/// in front of a window that must go on drawing. The worker blocks —
/// there is nothing for it to do until they decide — and every way this
/// can go wrong is a no.
#[test]
fn a_manifest_crosses_to_the_interface_and_silence_is_a_refusal() {
    let (mut discloser, inbox) = nacelle_ai::over_channel();

    let interface = thread::spawn(move || {
        let first = inbox.recv_timeout(SOON).expect("the first manifest");
        assert!(first.manifest().render().contains("anthropic"));
        first.send();

        // Answered by being dropped: a window that closed, a dialog
        // that was lost, an interface that forgot. None of those is a
        // yes.
        drop(inbox.recv_timeout(SOON).expect("the second manifest"));
        inbox
    });

    let manifest = payload().manifest("anthropic", "you asked", Why::FirstEscalation);
    assert_eq!(discloser.disclose(&manifest), Consent::Send);

    match discloser.disclose(&manifest) {
        Consent::Refuse { reason } => {
            assert!(reason.expect("a reason").contains("without an answer"));
        }
        Consent::Send => panic!("a dropped request is not consent"),
    }

    // And with the interface gone there is nobody to ask at all, which
    // is the same answer again rather than a wait that never ends.
    drop(interface.join().expect("the interface thread"));
    assert!(matches!(
        discloser.disclose(&manifest),
        Consent::Refuse { .. }
    ));
}

// ---- the background watch --------------------------------------------

/// A check that counts how often it was asked and fires only when a test
/// says so. It stands in for the cheap deterministic checks that run
/// continuously; what it proves is that they DO run continuously, and
/// that nothing downstream hears from them when nothing happened.
struct Trip {
    looked: Arc<AtomicU32>,
    armed: Arc<AtomicBool>,
}

impl Check for Trip {
    fn name(&self) -> &str {
        "trip"
    }

    fn look(&mut self) -> Option<Observation> {
        self.looked.fetch_add(1, Ordering::SeqCst);
        self.armed
            .swap(false, Ordering::SeqCst)
            .then(|| Observation::new("trip", "the thing happened"))
    }
}

fn wait_until(condition: impl Fn() -> bool) {
    let deadline = Instant::now() + SOON;
    while !condition() {
        assert!(Instant::now() < deadline, "waited too long");
        thread::sleep(Duration::from_millis(2));
    }
}

/// The shape of the whole design in one test: the cheap checks run all
/// the time, and nothing wakes up until one of them fires.
#[test]
fn the_watch_sleeps_until_a_check_fires_and_then_reports_once() {
    let looked = Arc::new(AtomicU32::new(0));
    let armed = Arc::new(AtomicBool::new(false));
    let (watch, events) = Watch::start(
        vec![Box::new(Trip {
            looked: Arc::clone(&looked),
            armed: Arc::clone(&armed),
        })],
        EVERY,
    )
    .expect("the supervisor thread");

    // The deterministic half is running.
    wait_until(|| looked.load(Ordering::SeqCst) > 3);

    // The expensive half is not: nothing has happened, so there is
    // nothing on the channel to wake a model with.
    assert_eq!(
        events.recv_timeout(Duration::from_millis(120)),
        Err(RecvTimeoutError::Timeout),
        "the watch must report nothing when nothing happened"
    );

    armed.store(true, Ordering::SeqCst);
    let observation = events.recv_timeout(SOON).expect("the event must arrive");
    assert_eq!(observation.check, "trip");
    assert_eq!(observation.summary, "the thing happened");

    // Once, not once per round.
    assert_eq!(
        events.recv_timeout(Duration::from_millis(120)),
        Err(RecvTimeoutError::Timeout),
        "one event, not one per round"
    );

    watch.stop();
}

/// It must be pausable and stoppable from the interface, and it must say
/// which of those it is — a background process that reads the user's
/// files and cannot be seen or stopped is not a feature.
#[test]
fn the_watch_can_be_paused_stopped_and_asked_what_it_is_doing() {
    let looked = Arc::new(AtomicU32::new(0));
    let (watch, _events) = Watch::start(
        vec![Box::new(Trip {
            looked: Arc::clone(&looked),
            armed: Arc::new(AtomicBool::new(false)),
        })],
        EVERY,
    )
    .expect("the supervisor thread");

    assert_eq!(watch.status(), Status::Running);
    assert!(watch.describe().contains("running"), "{}", watch.describe());
    assert!(watch.describe().contains("trip"), "{}", watch.describe());
    wait_until(|| looked.load(Ordering::SeqCst) > 2);

    let handle = watch.handle();
    handle.pause();
    assert_eq!(watch.status(), Status::Paused);
    assert!(watch.describe().contains("paused"), "{}", watch.describe());

    // Paused means the checks do not run, not that they run and are
    // ignored.
    thread::sleep(Duration::from_millis(60));
    let while_paused = looked.load(Ordering::SeqCst);
    thread::sleep(Duration::from_millis(60));
    assert_eq!(
        looked.load(Ordering::SeqCst),
        while_paused,
        "a paused watch must not be reading the machine"
    );

    handle.resume();
    wait_until(|| looked.load(Ordering::SeqCst) > while_paused);

    watch.stop();
    assert_eq!(handle.status(), Status::Stopped);

    // A stop stays stopped: resume through a handle somebody kept must
    // not bring back a watcher the user turned off.
    handle.resume();
    assert_eq!(handle.status(), Status::Stopped);
}

/// The two sources the design names: a threshold crossed in telemetry
/// the desktop already collects, and a widget reporting its own anomaly.
#[test]
fn a_threshold_fires_on_the_crossing_and_a_widget_can_report_directly() {
    let load = Arc::new(AtomicU32::new(10));
    let (reports, reporter) = Reports::new("widgets");
    let (watch, events) = Watch::start(
        vec![
            Box::new(Threshold::new("load", "the load average", 90, {
                let load = Arc::clone(&load);
                move || u64::from(load.load(Ordering::SeqCst))
            })),
            Box::new(reports),
        ],
        EVERY,
    )
    .expect("the supervisor thread");

    assert_eq!(
        events.recv_timeout(Duration::from_millis(120)),
        Err(RecvTimeoutError::Timeout),
        "a value under the limit is not an event"
    );

    load.store(99, Ordering::SeqCst);
    let crossed = events.recv_timeout(SOON).expect("the crossing");
    assert_eq!(crossed.check, "load");
    assert_eq!(crossed.detail.as_deref(), Some("99"));

    // Still over, and still not an event: a supervisor that reported a
    // full disk every round would wake the model every round.
    assert_eq!(
        events.recv_timeout(Duration::from_millis(120)),
        Err(RecvTimeoutError::Timeout),
        "staying over the limit is not a second event"
    );

    // It re-arms when the value comes back under, so the next time is
    // reported too.
    load.store(10, Ordering::SeqCst);
    thread::sleep(Duration::from_millis(60));
    load.store(120, Ordering::SeqCst);
    assert_eq!(events.recv_timeout(SOON).expect("the second crossing").check, "load");

    // And anything on the machine can wake it directly.
    reporter.report(Observation::new("clock", "the panel stopped repainting"));
    let reported = events.recv_timeout(SOON).expect("the widget's report");
    assert_eq!(reported.summary, "the panel stopped repainting");

    watch.stop();
}
