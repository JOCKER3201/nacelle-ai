//! The agent loop, against a backend that says what it was told to say.
//!
//! No network, no provider, no model: every reply here is written down
//! in advance, which is the only way to test the parts that matter.
//! What is being checked is not that a model answers well — it is that
//! the loop stops when it should, runs nothing the user did not agree
//! to, and never leaves a conversation the provider would refuse to
//! take back.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nacelle_ai::{
    Agent, AgentError, AgentEvent, ApprovalRequest, Approver, Backend, BackendError, Change,
    Content, Decision, Effect, EnvironmentFact, EventSink, Flow, History, Limits, Message, NoTools,
    Request, Role, StopReason, StreamEvent, ToolCall, ToolDeclaration, ToolOutput, ToolRegistry,
    Usage, Worker,
};
use serde_json::{json, Value};

const MODEL: &str = "test-model";

// ---------------------------------------------------------------- the
// backend: a script

/// One turn of the script.
#[derive(Clone)]
enum Reply {
    Stream(Vec<StreamEvent>),
    Fail(BackendError),
}

/// A backend that replays turns written down in advance and keeps every
/// request it was given, so a test can assert on what the model was
/// actually told.
struct Script {
    replies: VecDeque<Reply>,
    seen: Arc<Mutex<Vec<Request>>>,
    /// Repeat the last turn for ever. The only way to test a ceiling is
    /// against something that would otherwise go on without one.
    endless: bool,
    /// Held closed by a test that needs the stream to pause where it
    /// can act on it. `None` when nothing has to wait.
    gate: Option<Arc<AtomicBool>>,
}

impl Script {
    fn new(replies: Vec<Reply>) -> Self {
        Script {
            replies: replies.into(),
            seen: Arc::new(Mutex::new(Vec::new())),
            endless: false,
            gate: None,
        }
    }

    fn endless(mut self) -> Self {
        self.endless = true;
        self
    }

    fn gated(mut self, gate: Arc<AtomicBool>) -> Self {
        self.gate = Some(gate);
        self
    }

    fn seen(&self) -> Arc<Mutex<Vec<Request>>> {
        Arc::clone(&self.seen)
    }
}

impl Backend for Script {
    fn name(&self) -> &str {
        "script"
    }

    fn send(&mut self, request: &Request, sink: &mut EventSink<'_>) -> Result<(), BackendError> {
        self.seen.lock().expect("recorded requests").push(request.clone());

        let reply = if self.endless && self.replies.len() == 1 {
            self.replies.front().cloned().expect("a turn")
        } else {
            self.replies.pop_front().expect("the script ran out of turns")
        };

        match reply {
            Reply::Fail(err) => Err(err),
            Reply::Stream(events) => {
                for (index, event) in events.into_iter().enumerate() {
                    // The pause is after the first fragment: a test that
                    // wants to cancel mid-stream needs somewhere the
                    // stream is definitely still open.
                    if index == 2 {
                        if let Some(gate) = &self.gate {
                            let deadline = Instant::now() + Duration::from_secs(5);
                            while !gate.load(Ordering::SeqCst) {
                                assert!(Instant::now() < deadline, "the gate was never opened");
                                thread::sleep(Duration::from_millis(1));
                            }
                        }
                    }
                    // Exactly what a real backend does with Flow::Stop:
                    // stop reading, and never emit End.
                    if sink(event) == Flow::Stop {
                        return Err(BackendError::Cancelled);
                    }
                }
                Ok(())
            }
        }
    }
}

fn start() -> StreamEvent {
    StreamEvent::Start {
        model: MODEL.to_string(),
    }
}

fn end(stop: StopReason) -> StreamEvent {
    StreamEvent::End {
        stop,
        usage: Usage::default(),
    }
}

fn wants(name: &str, id: &str, input: Value) -> StreamEvent {
    StreamEvent::ToolCall(ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        input,
    })
}

/// A turn that is nothing but an answer.
fn says(text: &str) -> Reply {
    Reply::Stream(vec![
        start(),
        StreamEvent::Text(text.to_string()),
        end(StopReason::EndTurn),
    ])
}

/// A turn that narrates and then asks for tools.
fn asks_for(text: &str, calls: Vec<StreamEvent>) -> Reply {
    let mut events = vec![start(), StreamEvent::Text(text.to_string())];
    events.extend(calls);
    events.push(end(StopReason::ToolUse));
    Reply::Stream(events)
}

// ------------------------------------------------------------- the
// registry: a desktop that can be looked at and changed

/// Two tools: one that reads and one that changes something, which is
/// the whole distinction the approval path turns on.
struct Desk {
    ran: Arc<Mutex<Vec<String>>>,
}

impl Desk {
    fn new() -> Self {
        Desk {
            ran: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn ran(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.ran)
    }
}

impl ToolRegistry for Desk {
    fn declarations(&self) -> Vec<ToolDeclaration> {
        vec![
            ToolDeclaration::new(
                "list_themes",
                "List the themes installed on this machine.",
                json!({ "type": "object", "properties": {} }),
            ),
            ToolDeclaration::new(
                "set_theme",
                "Switch the desktop to a theme.",
                json!({
                    "type": "object",
                    "properties": { "name": { "type": "string" } },
                    "required": ["name"],
                }),
            ),
        ]
    }

    fn environment(&self) -> Vec<EnvironmentFact> {
        vec![
            EnvironmentFact::new("themes")
                .with_note("The desktop's whole look is a theme file.")
                .with_items(["default", "amber", "void"]),
            EnvironmentFact::new("layouts").with_items(["cockpit"]),
        ]
    }

    fn effect(&self, call: &ToolCall) -> Effect {
        match call.name.as_str() {
            "set_theme" => Effect::Change(
                Change::new(format!(
                    "set the theme to {}",
                    call.input["name"].as_str().unwrap_or("?")
                ))
                .with_detail("~/.config/nacelle-desktop/nacelle.conf"),
            ),
            _ => Effect::Read,
        }
    }

    fn invoke(&mut self, call: &ToolCall) -> ToolOutput {
        self.ran.lock().expect("ran").push(call.name.clone());
        match call.name.as_str() {
            "list_themes" => ToolOutput::ok("default, amber, void"),
            "set_theme" => ToolOutput::ok("the theme is now in place"),
            other => ToolOutput::error(format!("there is no tool called {other:?}")),
        }
    }
}

// --------------------------------------------------------------- the
// approvers

struct Yes;

impl Approver for Yes {
    fn approve(&mut self, _request: ApprovalRequest<'_>) -> Decision {
        Decision::Allow
    }
}

/// Says no, and remembers what it was asked about.
struct No {
    asked: Vec<String>,
    reason: Option<String>,
}

impl No {
    fn new() -> Self {
        No {
            asked: Vec::new(),
            reason: None,
        }
    }

    fn because(reason: &str) -> Self {
        No {
            asked: Vec::new(),
            reason: Some(reason.to_string()),
        }
    }
}

impl Approver for No {
    fn approve(&mut self, request: ApprovalRequest<'_>) -> Decision {
        self.asked.push(request.change.summary.clone());
        match &self.reason {
            Some(reason) => Decision::deny(reason),
            None => Decision::denied(),
        }
    }
}

fn quiet(_event: AgentEvent) -> Flow {
    Flow::Continue
}

fn agent(script: Script, desk: Desk) -> Agent {
    Agent::new(Box::new(script), Box::new(desk), MODEL)
}

/// The tool results in one message, in order.
fn results_of(message: &Message) -> Vec<(&str, &str, bool)> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            Content::ToolResult {
                id,
                output,
                is_error,
            } => Some((id.as_str(), output.as_str(), *is_error)),
            _ => None,
        })
        .collect()
}

// ------------------------------------------------------------ the loop

#[test]
fn a_plain_answer_is_one_turn() {
    let script = Script::new(vec![says("The theme is amber.")]);
    let seen = script.seen();
    let mut agent = agent(script, Desk::new());

    let done = agent
        .ask("which theme?", &mut Yes, &mut quiet)
        .expect("an answer");

    assert_eq!(done.text, "The theme is amber.");
    assert_eq!(done.turns, 1);
    assert_eq!(done.tools_run, 0);
    assert_eq!(done.stop, StopReason::EndTurn);
    assert_eq!(seen.lock().unwrap().len(), 1, "one request, no tool round");
}

#[test]
fn a_tool_runs_and_its_result_goes_back_to_the_model() {
    let script = Script::new(vec![
        asks_for("Let me look.", vec![wants("list_themes", "call-1", json!({}))]),
        says("You have three: default, amber and void."),
    ]);
    let seen = script.seen();
    let desk = Desk::new();
    let ran = desk.ran();

    let mut agent = agent(script, desk);
    let done = agent
        .ask("what themes do I have?", &mut Yes, &mut quiet)
        .expect("an answer");

    assert_eq!(ran.lock().unwrap().as_slice(), ["list_themes"]);
    assert_eq!(done.turns, 2);
    assert_eq!(done.tools_run, 1);
    assert_eq!(done.text, "You have three: default, amber and void.");

    // The second request has to carry the whole round: the question,
    // the assistant's call, and the result beside it.
    let seen = seen.lock().unwrap();
    let second = &seen[1];
    assert_eq!(second.messages.len(), 3);
    assert_eq!(second.messages[0].role, Role::User);
    assert_eq!(second.messages[1].role, Role::Assistant);
    assert!(matches!(
        second.messages[1].content.last(),
        Some(Content::ToolUse(call)) if call.id == "call-1"
    ));
    assert_eq!(second.messages[2].role, Role::User);
    assert_eq!(
        results_of(&second.messages[2]),
        [("call-1", "default, amber, void", false)]
    );
    assert!(agent.history().pairs_are_complete());
}

#[test]
fn every_tool_in_a_turn_runs_and_the_results_travel_together() {
    // Three calls in one turn. All of them run, and all the results go
    // back in a single message — a provider given them split across
    // several messages rejects the request.
    let script = Script::new(vec![
        asks_for(
            "Checking.",
            vec![
                wants("list_themes", "call-1", json!({})),
                wants("set_theme", "call-2", json!({ "name": "amber" })),
                wants("list_themes", "call-3", json!({})),
            ],
        ),
        says("Done."),
    ]);
    let seen = script.seen();
    let desk = Desk::new();
    let ran = desk.ran();

    let mut agent = agent(script, desk);
    let done = agent.ask("switch to amber", &mut Yes, &mut quiet).expect("an answer");

    assert_eq!(
        ran.lock().unwrap().as_slice(),
        ["list_themes", "set_theme", "list_themes"],
        "every call ran, in the order the model made them"
    );
    assert_eq!(done.tools_run, 3);

    let seen = seen.lock().unwrap();
    let results = &seen[1].messages[2];
    assert_eq!(results.role, Role::User);
    assert_eq!(
        results_of(results).iter().map(|r| r.0).collect::<Vec<_>>(),
        ["call-1", "call-2", "call-3"],
        "one message, one result per call, same order"
    );
}

#[test]
fn a_read_only_tool_is_never_put_to_the_user() {
    // The registry says list_themes only reads, so nobody is asked. An
    // agent that asked about every call would train the user to click
    // yes without reading, which is how the approval stops meaning
    // anything.
    let script = Script::new(vec![
        asks_for("Looking.", vec![wants("list_themes", "call-1", json!({}))]),
        says("Three."),
    ]);
    let desk = Desk::new();
    let ran = desk.ran();
    let mut refuser = No::new();

    let mut agent = agent(script, desk);
    agent
        .ask("what themes?", &mut refuser, &mut quiet)
        .expect("an answer");

    assert!(refuser.asked.is_empty(), "nobody was asked");
    assert_eq!(ran.lock().unwrap().as_slice(), ["list_themes"]);
}

#[test]
fn a_change_is_put_to_the_user_before_it_runs() {
    let script = Script::new(vec![
        asks_for(
            "Switching.",
            vec![wants("set_theme", "call-1", json!({ "name": "amber" }))],
        ),
        says("Done."),
    ]);
    let desk = Desk::new();
    let ran = desk.ran();

    let mut asked: Vec<String> = Vec::new();
    let mut agent = agent(script, desk);
    {
        // The closure form of Approver, which is what a terminal prompt
        // writes rather than a type. Scoped, because it borrows `asked`.
        let mut approver = |request: ApprovalRequest<'_>| {
            asked.push(request.change.summary.clone());
            assert_eq!(request.call.name, "set_theme");
            assert_eq!(
                request.change.detail.as_deref(),
                Some("~/.config/nacelle-desktop/nacelle.conf")
            );
            Decision::Allow
        };
        agent
            .ask("use amber", &mut approver, &mut quiet)
            .expect("an answer");
    }

    assert_eq!(asked, ["set the theme to amber"]);
    assert_eq!(ran.lock().unwrap().as_slice(), ["set_theme"]);
}

#[test]
fn a_refused_change_is_told_to_the_model_and_the_exchange_carries_on() {
    // A refusal is not a failure of the turn: the tool does not run,
    // the model is told in words that the user said no, and it gets to
    // answer with that knowledge.
    let script = Script::new(vec![
        asks_for(
            "Switching.",
            vec![wants("set_theme", "call-1", json!({ "name": "amber" }))],
        ),
        says("All right — I have left it as it was."),
    ]);
    let seen = script.seen();
    let desk = Desk::new();
    let ran = desk.ran();
    let mut refuser = No::because("I like the one I have");

    let mut agent = agent(script, desk);
    let done = agent
        .ask("use amber", &mut refuser, &mut quiet)
        .expect("the exchange finished despite the refusal");

    assert!(ran.lock().unwrap().is_empty(), "the tool never ran");
    assert_eq!(done.tools_run, 0, "a refused tool did not run");
    assert_eq!(done.text, "All right — I have left it as it was.");

    let seen = seen.lock().unwrap();
    let results = results_of(&seen[1].messages[2]);
    assert_eq!(results.len(), 1);
    let (id, told, is_error) = results[0];
    assert_eq!(id, "call-1");
    assert!(is_error, "the model has to be able to tell this apart from a result");
    assert!(told.contains("declined"), "{told}");
    assert!(told.contains("I like the one I have"), "{told}");
    assert!(
        told.contains("Do not call it again"),
        "the model is told what to do instead: {told}"
    );
}

#[test]
fn a_refusal_without_a_reason_still_says_who_refused() {
    let script = Script::new(vec![
        asks_for(
            "Switching.",
            vec![wants("set_theme", "call-1", json!({ "name": "amber" }))],
        ),
        says("Understood."),
    ]);
    let seen = script.seen();
    let mut agent = agent(script, Desk::new());
    agent
        .ask("use amber", &mut No::new(), &mut quiet)
        .expect("an answer");

    let seen = seen.lock().unwrap();
    let (_, told, _) = results_of(&seen[1].messages[2])[0];
    assert!(told.contains("The user declined"), "{told}");
}

#[test]
fn the_loop_gives_up_after_the_turn_limit() {
    // A model that answers every tool result with another tool call.
    // Without a ceiling this is a program that never returns and a bill
    // that never stops.
    let script = Script::new(vec![asks_for(
        "Again.",
        vec![wants("list_themes", "call-1", json!({}))],
    )])
    .endless();
    let seen = script.seen();

    let mut agent = agent(script, Desk::new()).with_limits(Limits {
        max_turns: 4,
        ..Limits::default()
    });

    let err = agent
        .ask("go", &mut Yes, &mut quiet)
        .expect_err("the ceiling is reached");

    assert_eq!(err, AgentError::TurnLimit { limit: 4 });
    assert_eq!(seen.lock().unwrap().len(), 4, "exactly the ceiling, not one more");
    assert!(err.to_string().contains("kept asking for tools"), "{err}");
    // The stop is clean: the conversation can still be sent.
    assert!(agent.history().pairs_are_complete());
}

#[test]
fn cancelling_mid_stream_ends_the_turn_without_an_answer() {
    let script = Script::new(vec![says("This answer will not be finished.")]);
    let mut agent = agent(script, Desk::new());

    let mut seen_text = false;
    let err = agent
        .ask("hello", &mut Yes, &mut |event| {
            if let AgentEvent::Text(_) = event {
                seen_text = true;
                return Flow::Stop;
            }
            Flow::Continue
        })
        .expect_err("stopped");

    assert!(seen_text);
    assert_eq!(err, AgentError::Cancelled);
    // Nothing of the half-finished turn was kept, so the question is
    // still the last thing said and asking again works.
    assert_eq!(agent.history().len(), 1);
    assert_eq!(agent.history().messages()[0].role, Role::User);
    assert!(agent.history().pairs_are_complete());
}

#[test]
fn cancelling_between_tools_still_leaves_every_call_answered() {
    // The hard case: three calls, stopped after the first. The two that
    // never ran still need results, or the next request is refused for
    // a tool_use with nothing beside it.
    let script = Script::new(vec![asks_for(
        "Working.",
        vec![
            wants("list_themes", "call-1", json!({})),
            wants("list_themes", "call-2", json!({})),
            wants("list_themes", "call-3", json!({})),
        ],
    )]);
    let desk = Desk::new();
    let ran = desk.ran();
    let mut agent = agent(script, desk);

    let mut started = 0;
    let err = agent
        .ask("go", &mut Yes, &mut |event| {
            if let AgentEvent::ToolStarted { .. } = event {
                started += 1;
                if started == 2 {
                    return Flow::Stop;
                }
            }
            Flow::Continue
        })
        .expect_err("stopped");

    assert_eq!(err, AgentError::Cancelled);
    assert_eq!(ran.lock().unwrap().as_slice(), ["list_themes"], "only the first ran");

    let history = agent.history();
    assert!(history.pairs_are_complete(), "every call has a result");
    let last = history.messages().last().expect("the results");
    let results = results_of(last);
    assert_eq!(results.len(), 3);
    assert!(!results[0].2, "the one that ran is not an error");
    assert!(results[1].1.contains("not run"), "{}", results[1].1);
    assert!(results[2].1.contains("not run"), "{}", results[2].1);
}

#[test]
fn cancelling_at_an_approval_stops_the_exchange() {
    let script = Script::new(vec![asks_for(
        "Switching.",
        vec![wants("set_theme", "call-1", json!({ "name": "amber" }))],
    )]);
    let desk = Desk::new();
    let ran = desk.ran();
    let mut agent = agent(script, desk);

    let mut approver = |_: ApprovalRequest<'_>| Decision::Cancel;
    let err = agent
        .ask("use amber", &mut approver, &mut quiet)
        .expect_err("stopped");

    assert_eq!(err, AgentError::Cancelled);
    assert!(ran.lock().unwrap().is_empty());
    assert!(agent.history().pairs_are_complete());
}

#[test]
fn a_failed_turn_leaves_the_question_askable_again() {
    let script = Script::new(vec![
        Reply::Fail(BackendError::Network("connection reset".to_string())),
        says("Sorry about that."),
    ]);
    let mut agent = agent(script, Desk::new());

    let err = agent
        .ask("hello", &mut Yes, &mut quiet)
        .expect_err("the turn failed");
    assert!(matches!(err, AgentError::Backend(BackendError::Network(_))));

    // The history is not littered with a half turn, so the second
    // attempt is a clean conversation rather than a broken one.
    assert_eq!(agent.history().len(), 1);
    let done = agent
        .ask("hello again", &mut Yes, &mut quiet)
        .expect("the retry works");
    assert_eq!(done.text, "Sorry about that.");
}

#[test]
fn a_stream_that_never_ended_is_not_reported_as_an_answer() {
    // A backend that returns Ok without emitting End has broken the
    // contract. Treating that as a finished turn would present a
    // truncated reply as a complete one.
    let script = Script::new(vec![Reply::Stream(vec![
        start(),
        StreamEvent::Text("half a thought".to_string()),
    ])]);
    let mut agent = agent(script, Desk::new());

    let err = agent.ask("hello", &mut Yes, &mut quiet).expect_err("refused");
    assert!(matches!(
        err,
        AgentError::Backend(BackendError::Protocol(_))
    ));
}

#[test]
fn an_unknown_tool_is_reported_to_the_model_not_to_the_caller() {
    // A model inventing a tool name is an everyday event and not a
    // reason to end the exchange: it is told, and it tries something
    // else.
    let script = Script::new(vec![
        asks_for("Trying.", vec![wants("reboot_the_sun", "call-1", json!({}))]),
        says("That one does not exist — here is what I can do instead."),
    ]);
    let seen = script.seen();
    let mut agent = agent(script, Desk::new());

    let done = agent.ask("go", &mut Yes, &mut quiet).expect("an answer");
    assert_eq!(done.turns, 2);

    let seen = seen.lock().unwrap();
    let (_, told, is_error) = results_of(&seen[1].messages[2])[0];
    assert!(is_error);
    assert!(told.contains("reboot_the_sun"), "{told}");
}

#[test]
fn an_agent_with_no_tools_still_answers() {
    let script = Script::new(vec![says("I cannot do that, but I can talk about it.")]);
    let mut agent = Agent::new(Box::new(script), Box::new(NoTools), MODEL);

    let done = agent.ask("hello", &mut Yes, &mut quiet).expect("an answer");
    assert_eq!(done.tools_run, 0);
    assert!(agent.system_prompt().contains("nacelle"));
}

#[test]
fn usage_is_added_up_across_turns() {
    let usage = |input, output| StreamEvent::End {
        stop: StopReason::ToolUse,
        usage: Usage {
            input_tokens: input,
            output_tokens: output,
            ..Usage::default()
        },
    };
    let script = Script::new(vec![
        Reply::Stream(vec![
            start(),
            wants("list_themes", "call-1", json!({})),
            usage(100, 20),
        ]),
        Reply::Stream(vec![
            start(),
            StreamEvent::Text("Three.".to_string()),
            StreamEvent::End {
                stop: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 250,
                    output_tokens: 8,
                    cache_read_tokens: 40,
                    ..Usage::default()
                },
            },
        ]),
    ]);
    let mut agent = agent(script, Desk::new());

    let done = agent.ask("what themes?", &mut Yes, &mut quiet).expect("an answer");
    assert_eq!(done.usage.input_tokens, 350);
    assert_eq!(done.usage.output_tokens, 28);
    assert_eq!(done.usage.cache_read_tokens, 40);
    assert_eq!(done.usage.total_tokens(), 418);
}

// -------------------------------------------------------- the prompt

#[test]
fn the_system_prompt_is_built_from_the_registry() {
    let script = Script::new(vec![says("hello")]);
    let agent = agent(script, Desk::new());

    let prompt = agent.system_prompt();
    // What is in it came from the registry, not from a list written
    // into the crate: a machine with other themes gets other words.
    assert!(prompt.contains("amber"), "{prompt}");
    assert!(prompt.contains("cockpit"), "{prompt}");
    assert!(prompt.contains("themes"), "{prompt}");
    // And the rules the model cannot observe for itself.
    assert!(prompt.contains("user's decision"), "{prompt}");
}

#[test]
fn a_role_is_folded_into_the_prompt_without_losing_the_environment() {
    let script = Script::new(vec![says("hello")]);
    let agent = agent(script, Desk::new()).with_role("You are the panel in the corner.");

    assert!(agent.system_prompt().contains("panel in the corner"));
    assert!(agent.system_prompt().contains("amber"));
}

#[test]
fn the_system_prompt_is_the_same_bytes_on_every_turn() {
    // The provider's cache is matched on exact bytes, so a prompt that
    // drifted between turns would be paid for in full every time and
    // nothing would say so.
    let script = Script::new(vec![
        asks_for("Looking.", vec![wants("list_themes", "call-1", json!({}))]),
        asks_for("Again.", vec![wants("list_themes", "call-2", json!({}))]),
        says("Done."),
    ]);
    let seen = script.seen();
    let mut agent = agent(script, Desk::new());
    agent.ask("go", &mut Yes, &mut quiet).expect("an answer");

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    let first = seen[0].system.as_deref().expect("a system prompt");
    for request in seen.iter() {
        assert_eq!(request.system.as_deref(), Some(first));
    }
}

#[test]
fn the_tools_are_declared_to_the_backend() {
    let script = Script::new(vec![says("hello")]);
    let seen = script.seen();
    let mut agent = agent(script, Desk::new());
    agent.ask("go", &mut Yes, &mut quiet).expect("an answer");

    let seen = seen.lock().unwrap();
    let names: Vec<&str> = seen[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["list_themes", "set_theme"]);
}

// ------------------------------------------------------- the history

fn user(text: &str) -> Message {
    Message::user(text)
}

fn call_and_result(id: &str) -> (Message, Message) {
    (
        Message::new(
            Role::Assistant,
            vec![Content::ToolUse(ToolCall {
                id: id.to_string(),
                name: "list_themes".to_string(),
                input: json!({}),
            })],
        ),
        Message::new(
            Role::User,
            vec![Content::ToolResult {
                id: id.to_string(),
                output: "default, amber, void".to_string(),
                is_error: false,
            }],
        ),
    )
}

/// Three exchanges, each of them a question, a tool round and an
/// answer.
fn conversation() -> History {
    let mut history = History::new(usize::MAX);
    for round in 0..3 {
        history.push(user(&format!("question {round} {}", "x".repeat(200))));
        let (call, result) = call_and_result(&format!("call-{round}"));
        history.push(call);
        history.push(result);
        history.push(Message::assistant(format!("answer {round}")));
    }
    history
}

#[test]
fn a_history_under_its_budget_is_left_alone() {
    let mut history = conversation();
    let before = history.len();
    assert_eq!(history.trim(), 0);
    assert_eq!(history.len(), before);
}

#[test]
fn trimming_drops_the_oldest_exchange_whole() {
    let mut history = conversation();
    let full = history.size();
    // Room for about two of the three exchanges.
    history.set_budget(full * 2 / 3);

    let dropped = history.trim();
    assert_eq!(dropped, 4, "the whole of the oldest exchange, nothing else");
    assert!(history.size() <= history.budget());
    assert!(
        history.messages()[0].text().starts_with("question 1"),
        "the oldest question went with its answer"
    );
    // And what is left still begins the way a provider demands.
    assert_eq!(history.messages()[0].role, Role::User);
}

#[test]
fn trimming_never_separates_a_call_from_its_result() {
    // The invariant. Every budget from "everything fits" down to
    // "almost nothing does" has to leave a conversation that can be
    // sent — a tool_use with no result beside it is a 400, and a
    // result with no call before it is the same.
    let full = conversation().size();
    for budget in (0..=full).step_by(37) {
        let mut history = conversation();
        history.set_budget(budget);
        history.trim();

        assert!(
            history.pairs_are_complete(),
            "budget {budget} broke a tool call away from its result"
        );
        assert_eq!(
            history.messages()[0].role,
            Role::User,
            "budget {budget} left the conversation starting with the wrong role"
        );
        assert!(
            !history.messages()[0]
                .content
                .iter()
                .any(|block| matches!(block, Content::ToolResult { .. })),
            "budget {budget} left a result whose call had been dropped"
        );
    }
}

#[test]
fn the_newest_exchange_survives_a_budget_it_cannot_fit() {
    let mut history = conversation();
    history.set_budget(1);
    history.trim();

    assert!(!history.is_empty(), "the question being answered is never dropped");
    assert!(history.size() > history.budget(), "and the budget gives way, not the exchange");
    assert!(history.messages()[0].text().starts_with("question 2"));
    assert!(history.pairs_are_complete());
}

#[test]
fn a_new_question_joins_the_tool_results_rather_than_following_them() {
    // What happens when the previous exchange stopped on tool results
    // and the user types again: two user messages in a row is a 400 on
    // Anthropic, so they are one message.
    let mut history = History::new(usize::MAX);
    history.push(user("first"));
    let (call, result) = call_and_result("call-1");
    history.push(call);
    history.push(result);
    history.push(user("second"));

    assert_eq!(history.len(), 3);
    let last = history.messages().last().unwrap();
    assert_eq!(last.role, Role::User);
    assert_eq!(last.content.len(), 2, "the result and the new question together");
    assert_eq!(last.text(), "second");
    assert!(history.pairs_are_complete());
}

#[test]
fn leading_instructions_are_never_trimmed_away() {
    let mut history = History::new(usize::MAX);
    history.push(Message::system("Always answer in Polish."));
    for round in 0..3 {
        history.push(user(&format!("question {round} {}", "x".repeat(200))));
        history.push(Message::assistant(format!("answer {round}")));
    }
    history.set_budget(400);
    history.trim();

    assert_eq!(history.messages()[0].role, Role::System);
    assert_eq!(history.messages()[0].text(), "Always answer in Polish.");
}

// -------------------------------------------------------- the worker

/// Drain until the exchange ends, keeping what arrived.
///
/// Keeping is the catch: an [`AgentEvent::Approval`] held in the
/// returned vector is an approval that has not been dropped, and the
/// worker is still waiting on it. Tests about approvals run their own
/// loop so they own that decision.
fn drain(
    events: &std::sync::mpsc::Receiver<AgentEvent>,
    mut handle: impl FnMut(&AgentEvent),
) -> Vec<AgentEvent> {
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "the worker never finished");
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => {
                handle(&event);
                let last = matches!(event, AgentEvent::Finished(_) | AgentEvent::Failed(_));
                seen.push(event);
                if last {
                    return seen;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return seen,
        }
    }
}

#[test]
fn the_worker_answers_on_a_thread_of_its_own() {
    let script = Script::new(vec![says("Hello from the worker.")]);
    let agent = Agent::new(Box::new(script), Box::new(Desk::new()), MODEL);
    let (worker, events) = Worker::spawn(agent).expect("a thread");

    worker.ask("hello").expect("queued");
    let seen = drain(&events, |_| {});

    match seen.last() {
        Some(AgentEvent::Finished(done)) => assert_eq!(done.text, "Hello from the worker."),
        other => panic!("expected a finished exchange, got {other:?}"),
    }
    assert!(seen
        .iter()
        .any(|event| matches!(event, AgentEvent::TurnStarted { turn: 1, .. })));
    worker.shutdown();
}

#[test]
fn an_approval_travels_to_the_interface_and_back() {
    let script = Script::new(vec![
        asks_for(
            "Switching.",
            vec![wants("set_theme", "call-1", json!({ "name": "amber" }))],
        ),
        says("Done."),
    ]);
    let desk = Desk::new();
    let ran = desk.ran();
    let agent = Agent::new(Box::new(script), Box::new(desk), MODEL);
    let (worker, events) = Worker::spawn(agent).expect("a thread");

    worker.ask("use amber").expect("queued");

    // The interface answers as it drains, which is exactly what a
    // dialog in a winit loop does.
    let mut summaries = Vec::new();
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "the worker never finished");
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(AgentEvent::Approval(pending)) => {
                summaries.push(pending.change().summary.clone());
                assert_eq!(pending.call().name, "set_theme");
                pending.allow();
            }
            Ok(event) => {
                let last = matches!(event, AgentEvent::Finished(_) | AgentEvent::Failed(_));
                seen.push(event);
                if last {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    assert_eq!(summaries, ["set the theme to amber"]);
    assert_eq!(ran.lock().unwrap().as_slice(), ["set_theme"]);
    assert!(matches!(seen.last(), Some(AgentEvent::Finished(_))));
    worker.shutdown();
}

#[test]
fn an_approval_that_is_dropped_counts_as_no() {
    // An interface that loses the question must not be able to produce
    // a yes by accident.
    let script = Script::new(vec![
        asks_for(
            "Switching.",
            vec![wants("set_theme", "call-1", json!({ "name": "amber" }))],
        ),
        says("Left as it was."),
    ]);
    let seen_requests = script.seen();
    let desk = Desk::new();
    let ran = desk.ran();
    let agent = Agent::new(Box::new(script), Box::new(desk), MODEL);
    let (worker, events) = Worker::spawn(agent).expect("a thread");

    worker.ask("use amber").expect("queued");

    let mut ended = None;
    let mut dropped = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    while ended.is_none() {
        assert!(Instant::now() < deadline, "the worker never finished");
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(AgentEvent::Approval(pending)) => {
                assert_eq!(pending.call().name, "set_theme");
                // The interface goes away without answering. Nothing is
                // sent back at all — the worker only learns of it
                // because the request was destroyed.
                drop(pending);
                dropped += 1;
            }
            Ok(event @ (AgentEvent::Finished(_) | AgentEvent::Failed(_))) => ended = Some(event),
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    assert_eq!(dropped, 1);
    assert!(ran.lock().unwrap().is_empty(), "nothing ran unasked");
    assert!(matches!(ended, Some(AgentEvent::Finished(_))), "{ended:?}");

    let requests = seen_requests.lock().unwrap();
    let (_, told, is_error) = results_of(&requests[1].messages[2])[0];
    assert!(is_error);
    assert!(told.contains("declined"), "{told}");
    worker.shutdown();
}

#[test]
fn cancelling_stops_an_answer_in_flight() {
    // The stream is held open at a known point so this is a test and
    // not a race: the worker is stopped while the reply is still
    // arriving, and the exchange ends without an answer.
    let gate = Arc::new(AtomicBool::new(false));
    let script = Script::new(vec![Reply::Stream(vec![
        start(),
        StreamEvent::Text("this far".to_string()),
        StreamEvent::Text(" and no further".to_string()),
        end(StopReason::EndTurn),
    ])])
    .gated(Arc::clone(&gate));

    let agent = Agent::new(Box::new(script), Box::new(Desk::new()), MODEL);
    let (worker, events) = Worker::spawn(agent).expect("a thread");
    worker.ask("hello").expect("queued");

    let stop = worker.cancel_handle();
    let seen = drain(&events, |event| {
        if let AgentEvent::Text(_) = event {
            stop.cancel();
            gate.store(true, Ordering::SeqCst);
        }
    });

    match seen.last() {
        Some(AgentEvent::Failed(AgentError::Cancelled)) => {}
        other => panic!("expected a cancelled exchange, got {other:?}"),
    }
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, AgentEvent::Finished(_))),
        "a stopped exchange must never look like a finished one"
    );
    worker.shutdown();
}

#[test]
fn a_question_stopped_before_it_started_is_answered_all_the_same() {
    // Return then escape, faster than the worker got out of bed. The
    // interface still has to hear that the question is over.
    let script = Script::new(vec![says("never sent")]);
    let agent = Agent::new(Box::new(script), Box::new(Desk::new()), MODEL);
    let (worker, events) = Worker::spawn(agent).expect("a thread");

    worker.ask("hello").expect("queued");
    worker.cancel();

    let seen = drain(&events, |_| {});
    assert!(
        matches!(
            seen.last(),
            Some(AgentEvent::Failed(AgentError::Cancelled)) | Some(AgentEvent::Finished(_))
        ),
        "the exchange ended one way or the other: {seen:?}"
    );
    worker.shutdown();
}

#[test]
fn a_worker_that_has_stopped_says_so_instead_of_swallowing_a_question() {
    let script = Script::new(vec![says("hello")]);
    let agent = Agent::new(Box::new(script), Box::new(Desk::new()), MODEL);
    let (worker, events) = Worker::spawn(agent).expect("a thread");
    drop(events);

    // The receiver is gone; the worker notices when it tries to report.
    worker.ask("hello").expect("queued all the same");
    worker.shutdown();
}
