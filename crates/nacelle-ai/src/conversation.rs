//! The conversation as the window shows it: a column of turns, and the
//! rows that column is drawn as.
//!
//! Nothing here draws and nothing here opens a window. It is the model
//! between [`AgentEvent`] and [`nacelle::view::list`], and it is written
//! apart from the frame for one reason: everything that can go wrong
//! about a streamed conversation — a fragment landing in the wrong turn,
//! a wrapped line attributed to the wrong speaker, the view jumping away
//! from where the reader put it — can then be tested without a GPU.
//!
//! Three rules shape it.
//!
//! **Fragments coalesce.** A model reply arrives as a run of
//! [`AgentEvent::Text`] whose boundaries mean nothing, so they are
//! appended to ONE open turn. A turn is closed by anything that changes
//! the subject — a tool, the end of the exchange, a failure — and the
//! next fragment opens a new one.
//!
//! **A turn is not a row.** The list this is drawn through is a row
//! list of one height ([`nacelle::view::list`]), so a turn becomes a
//! headline row plus one row per wrapped line, plus one blank row
//! between turns. The blank row is why this file names no gap: the
//! separation between two turns IS a list row, so it is `list.row_h`
//! and never a number chosen here.
//!
//! **The reader owns the offset.** [`Conversation::follows_tail`] is on
//! while the newest row should stay in view and off the moment the
//! reader scrolls away from the bottom. A window that scrolled to the
//! end on every fragment would make reading back through a long answer
//! impossible while it is still arriving.

use nacelle::ui::{self, Sev};
use nacelle::view::model::{RowBuf, Rows};
use nacelle::view::paint;
use nacelle::view::surface::Surface;

use nacelle_ai::{AgentError, AgentEvent, BackendError, Completion, StopReason};

/// Who is speaking.
///
/// The severity beside each is an INDEX into the theme's closed set, not
/// a colour: this file judges what a turn is, and `default.theme`
/// decides what that judgement looks like — the same contract a script
/// widget has with `severity.*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Voice {
    /// The person at the keyboard.
    You,
    /// The model answering.
    Model,
    /// A tool the model asked for, and what came of it.
    Tool,
    /// The program itself, saying what happened: a backend that is not
    /// there, a refusal, a stop, the turn ceiling.
    Notice,
}

/// One thing said, and everything the row list needs to draw it.
#[derive(Clone, Debug)]
pub struct Turn {
    pub voice: Voice,
    /// The headline row's label — who is speaking, or what happened.
    pub head: String,
    /// The body, which may be empty and which grows while a reply
    /// streams in. Newlines are kept; wrapping happens at draw time
    /// because it depends on the width and on the theme.
    pub body: String,
    /// The headline row's trailing text — a stop reason, a token count.
    pub status: String,
    /// The name of the severity role this turn reads as. One of
    /// [`nacelle::ui::SEVERITY_ROLES`]; a name outside that set resolves
    /// through the toolkit's own fallback rather than through `ok`.
    pub severity: &'static str,
}

/// The severity a turn of each voice carries when nothing worse is
/// known. `info` for a question ("notable, not a problem"), `ok` for an
/// answer that arrived, and a notice carries whatever it is about.
const SEV_YOU: &str = "info";
const SEV_MODEL: &str = "ok";
const SEV_TOOL: &str = "info";

/// What a notice is about. Each is a state the window must SHOW rather
/// than leave the reader to infer, and each says what to do about it —
/// see [`Notice::sentence`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// The local server did not answer.
    NoOllama { detail: String },
    /// No Anthropic credential could be resolved.
    NoCredential { detail: String },
    /// Neither provider can be reached, so nothing can be asked.
    NoBackend,
    /// The provider rejected the credential.
    AuthRejected { detail: String },
    /// The model declined the request. Not a transport failure.
    Refused { detail: String },
    /// The user stopped it.
    Cancelled,
    /// The loop hit [`nacelle_ai::Limits::max_turns`].
    TurnLimit { limit: u32 },
    /// Anything else the backend reported.
    Failed { detail: String },
    /// A tool ran, or was asked for, or was refused.
    Tool { head: String, detail: String },
    /// A change is waiting on the user.
    Approval { summary: String, detail: String },
    /// Who answers, and what the other half may be asked. Said once at
    /// the start, because it is a fact about the whole conversation.
    Supervision { detail: String },
    /// Nothing was sent: the session is pinned, the provider cannot be
    /// reached, or the user read the manifest and said no. The agent's
    /// own sentence about it, which always opens with what could not be
    /// done here.
    Withheld { detail: String },
    /// Layer 4: what is about to leave the machine, waiting on the
    /// user's word. `detail` is the manifest as the core rendered it —
    /// this window does not lay one out itself, because the manifest's
    /// wording is part of the guarantee rather than part of the view.
    Manifest { detail: String },
}

impl Notice {
    /// The headline row's label.
    fn head(&self) -> String {
        match self {
            Notice::NoOllama { .. } => "NO LOCAL MODEL".into(),
            Notice::NoCredential { .. } => "NO ANTHROPIC CREDENTIAL".into(),
            Notice::NoBackend => "NOTHING TO ASK".into(),
            Notice::AuthRejected { .. } => "CREDENTIAL REJECTED".into(),
            Notice::Refused { .. } => "THE MODEL DECLINED".into(),
            Notice::Cancelled => "STOPPED".into(),
            Notice::TurnLimit { .. } => "STOPPED AT THE TURN CEILING".into(),
            Notice::Failed { .. } => "THE TURN DID NOT FINISH".into(),
            Notice::Tool { head, .. } => head.clone(),
            Notice::Approval { .. } => "WAITING FOR YOU".into(),
            Notice::Supervision { .. } => "WHO ANSWERS".into(),
            Notice::Withheld { .. } => "NOTHING LEFT THIS MACHINE".into(),
            Notice::Manifest { .. } => "ABOUT TO LEAVE THIS MACHINE".into(),
        }
    }

    /// The body: what happened, and what to do about it. Every one of
    /// these is read by somebody who cannot see the code, so each says
    /// the second half out loud.
    fn sentence(&self) -> String {
        match self {
            Notice::NoOllama { detail } => format!(
                "{detail}\nStart it with `ollama serve`, or point OLLAMA_HOST at a machine \
                 that is running one."
            ),
            Notice::NoCredential { detail } => format!(
                "{detail}\nRun `claude setup-token` and put the token in ANTHROPIC_AUTH_TOKEN, \
                 or write an API key into ~/.config/nacelle-ai/credentials.json with mode 600."
            ),
            Notice::NoBackend => "Neither provider above can answer, so there is nothing to \
                 ask yet. Fix one of them and start the program again."
                .into(),
            Notice::AuthRejected { detail } => format!(
                "{detail}\nThe token was sent and refused, so a retry with the same one will \
                 be refused too. Mint another, or use the local backend, which needs none."
            ),
            Notice::Refused { detail } => format!(
                "{detail}\nThe request arrived and was understood; it was the model that said \
                 no. Ask for something else, or ask for it differently."
            ),
            Notice::Cancelled => "You stopped this turn. Whatever had arrived is above; the \
                 conversation is intact and the next question carries on from here."
                .into(),
            Notice::TurnLimit { limit } => format!(
                "The model was still asking for tools after {limit} turns, so the loop stopped \
                 rather than keep spending. Ask again to continue from where it got to."
            ),
            Notice::Failed { detail } => detail.clone(),
            Notice::Tool { detail, .. } => detail.clone(),
            Notice::Approval { summary, detail } => {
                if detail.is_empty() {
                    format!("{summary}\nEnter allows it, Escape declines and tells the model so.")
                } else {
                    format!(
                        "{summary}\n{detail}\nEnter allows it, Escape declines and tells the \
                         model so."
                    )
                }
            }
            Notice::Supervision { detail } | Notice::Withheld { detail } => detail.clone(),
            // The manifest arrives as text and is shown as text. The
            // last line is this window's, because it is the only part
            // that is about a keyboard.
            Notice::Manifest { detail } => format!(
                "{detail}\nEnter sends it. Escape keeps it here and tells the model that you \
                 said no."
            ),
        }
    }

    /// Which severity role the notice reads as.
    fn severity(&self) -> &'static str {
        match self {
            // "not reporting; absent, not zero" — exactly a server that
            // is not running.
            Notice::NoOllama { .. } | Notice::NoBackend => "offline",
            // "degraded, will become a problem": Anthropic is unusable
            // until a token exists, and the local backend may not be.
            Notice::NoCredential { .. } => "warning",
            // "failed or failing, act now".
            Notice::AuthRejected { .. } | Notice::Failed { .. } => "critical",
            // "a critical condition that has been bounded" — the
            // request was understood and stopped on purpose.
            // Both are a stop that held: the request was understood
            // and went no further on purpose.
            Notice::Refused { .. } | Notice::Withheld { .. } => "contained",
            Notice::Cancelled => "info",
            Notice::TurnLimit { .. } => "warning",
            Notice::Tool { .. } => SEV_TOOL,
            // Both are questions the program cannot answer for the
            // user, and one of them is the last moment before bytes
            // leave the machine.
            Notice::Approval { .. } | Notice::Manifest { .. } => "warning",
            Notice::Supervision { .. } => "info",
        }
    }
}

/// Everything said so far, and which turn the stream is filling.
pub struct Conversation {
    turns: Vec<Turn>,
    /// The turn arriving fragments are appended to, when one is open.
    open: Option<usize>,
    /// Bumped by every change. What tells a cached wrap that the text it
    /// wrapped is no longer the text there is.
    generation: u64,
    /// Whether the newest row should stay in view.
    follow: bool,
    /// A question has been sent and no [`AgentEvent::Finished`] or
    /// [`AgentEvent::Failed`] has come back for it.
    busy: bool,
    /// The model the provider says is answering — it is not always the
    /// one that was asked for, so it is taken from the stream.
    answering: Option<String>,
}

impl Default for Conversation {
    fn default() -> Self {
        Self::new()
    }
}

impl Conversation {
    pub fn new() -> Conversation {
        Conversation {
            turns: Vec::new(),
            open: None,
            generation: 0,
            follow: true,
            busy: false,
            answering: None,
        }
    }

    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    /// The rewrite counter — the cache key of anything derived from the
    /// text, which is the wrap.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether an exchange is in flight. The window shows it, and the
    /// key handler reads it: Escape means "stop that" only while it is
    /// true.
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// Which model the provider said is answering, once it has said.
    pub fn answering(&self) -> Option<&str> {
        self.answering.as_deref()
    }

    pub fn follows_tail(&self) -> bool {
        self.follow
    }

    /// The reader moved the view. Following the tail is on only while
    /// the view is AT the bottom — anywhere else and new text must not
    /// drag the page out from under them.
    pub fn set_follows_tail(&mut self, at_bottom: bool) {
        self.follow = at_bottom;
    }

    /// A question was sent. Sending is done from the bottom of the
    /// conversation by definition, so the view follows the tail again
    /// whatever the reader had scrolled to.
    pub fn asked(&mut self, question: &str) {
        self.close();
        self.push(Turn {
            voice: Voice::You,
            head: "YOU".into(),
            body: question.trim_end().to_string(),
            status: String::new(),
            severity: SEV_YOU,
        });
        self.busy = true;
        self.follow = true;
    }

    /// Something the program has to say for itself.
    pub fn note(&mut self, notice: Notice) {
        self.close();
        self.push(Turn {
            voice: Voice::Notice,
            head: notice.head(),
            body: notice.sentence(),
            status: String::new(),
            severity: notice.severity(),
        });
    }

    /// One event from the worker thread, folded into the column.
    ///
    /// Taken by reference because [`AgentEvent::Approval`] carries a
    /// request that has to be answered rather than read, and the window
    /// keeps that one; everything else is described here and dropped.
    pub fn apply(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TurnStarted { model, .. } => {
                // Not the opening of a turn on screen: one question can
                // cost several model turns (answer, tool, answer), and
                // splitting the reply at each of them would attribute
                // one sentence to two speakers. What it does say is who
                // is actually answering.
                self.answering = Some(model.clone());
                self.touch();
            }
            AgentEvent::Text(fragment) => {
                let head = self.model_head();
                self.append(Voice::Model, head, SEV_MODEL, fragment);
            }
            AgentEvent::Thinking(fragment) => {
                self.append(Voice::Model, "THINKING".into(), SEV_TOOL, fragment);
            }
            AgentEvent::ToolStarted { call } => {
                self.note(Notice::Tool {
                    head: format!("TOOL · {}", call.name),
                    detail: call.input.to_string(),
                });
            }
            AgentEvent::ToolFinished { name, is_error, .. } => {
                let head = if *is_error {
                    format!("TOOL FAILED · {name}")
                } else {
                    format!("TOOL DONE · {name}")
                };
                self.close();
                self.push(Turn {
                    voice: Voice::Tool,
                    head,
                    body: String::new(),
                    status: String::new(),
                    // A tool that failed is a result the model will read
                    // and work around, not a fault of the session.
                    severity: if *is_error { "warning" } else { SEV_TOOL },
                });
            }
            AgentEvent::ToolDenied { name, reason, .. } => {
                self.note(Notice::Tool {
                    head: format!("TOOL DECLINED · {name}"),
                    detail: reason.clone(),
                });
            }
            AgentEvent::Approval(_) => {
                // The window owns this one: it has to be answered, and
                // dropping it is a refusal. It reaches the column
                // through [`Conversation::note`] with
                // [`Notice::Approval`].
            }
            AgentEvent::Finished(completion) => self.finished(completion),
            AgentEvent::Failed(err) => self.failed(err),
        }
    }

    /// The exchange ended well.
    fn finished(&mut self, completion: &Completion) {
        self.busy = false;
        // A reply that ran out of room is a fragment, not an answer, and
        // the only place that is visible is here.
        if completion.stop == StopReason::MaxTokens {
            self.note(Notice::Failed {
                detail: "The reply hit the token ceiling and stops mid-thought. What is above \
                         is a fragment; ask again for the rest, or ask for less at a time."
                    .into(),
            });
        }
        // The cost of the exchange, on the turn it belongs to. Said
        // once, when it is known, rather than guessed at per fragment.
        let usage = completion.usage;
        if let Some(i) = self.last_model_turn() {
            self.turns[i].status = format!(
                "{} in · {} out",
                usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens,
                usage.output_tokens
            );
        }
        self.close();
    }

    /// The exchange did not end well.
    fn failed(&mut self, err: &AgentError) {
        self.busy = false;
        self.close();
        let notice = match err {
            AgentError::Cancelled => Notice::Cancelled,
            AgentError::TurnLimit { limit } => Notice::TurnLimit { limit: *limit },
            AgentError::Backend(BackendError::Auth(detail)) => Notice::AuthRejected {
                detail: detail.clone(),
            },
            AgentError::Backend(
                refused @ BackendError::Refused { .. },
            ) => Notice::Refused {
                detail: refused.to_string(),
            },
            // Not a failure and not the provider's doing: this machine
            // decided, and the sentence it decided with is already
            // written for the person reading it.
            AgentError::Backend(BackendError::Withheld(detail)) => Notice::Withheld {
                detail: detail.clone(),
            },
            AgentError::Backend(other) => Notice::Failed {
                detail: other.to_string(),
            },
        };
        self.note(notice);
    }

    /// The headline a model turn carries: the model the provider said is
    /// answering, or the plain word until it has said.
    fn model_head(&self) -> String {
        match &self.answering {
            Some(model) => model.to_uppercase(),
            None => "MODEL".into(),
        }
    }

    fn last_model_turn(&self) -> Option<usize> {
        self.turns.iter().rposition(|t| t.voice == Voice::Model)
    }

    /// Appends to the open turn when it is the same voice under the same
    /// headline, and opens one otherwise. This is where a run of
    /// fragments becomes one paragraph instead of one turn per packet.
    fn append(&mut self, voice: Voice, head: String, severity: &'static str, fragment: &str) {
        match self.open {
            Some(i) if self.turns[i].voice == voice && self.turns[i].head == head => {
                self.turns[i].body.push_str(fragment);
                self.touch();
            }
            _ => {
                self.push(Turn {
                    voice,
                    head,
                    body: fragment.to_string(),
                    status: String::new(),
                    severity,
                });
                self.open = Some(self.turns.len() - 1);
            }
        }
    }

    fn push(&mut self, turn: Turn) {
        self.turns.push(turn);
        self.open = None;
        self.touch();
    }

    /// Nothing more belongs in the turn that was being filled.
    fn close(&mut self) {
        self.open = None;
    }

    fn touch(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }
}

// ---------------------------------------------------------------------
// The rows
// ---------------------------------------------------------------------

/// The wrapped column, and what it was wrapped for.
///
/// Wrapping measures every line of every turn, so it is done when the
/// answer changes and not once a frame. The key carries everything the
/// result depends on: the theme (a new type role is a new measurement),
/// the conversation, the width it was fitted to and the stack's shrink.
#[derive(Default)]
pub struct RowCache {
    rows: Rows,
    key: Option<Key>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    epoch: u32,
    generation: u64,
    width: u32,
    shrink: u32,
}

impl RowCache {
    pub fn new() -> RowCache {
        RowCache::default()
    }

    /// The rows as the list draws them. Valid after [`RowCache::build`].
    pub fn rows(&self) -> &Rows {
        &self.rows
    }

    /// Rebuilds the column if anything it depends on has moved.
    ///
    /// `width` is the list's own width — the same rectangle
    /// [`nacelle::view::list::list`] is given — and the body width is
    /// derived from it here with the very tokens that view lays a row
    /// out with, so a line is wrapped to the width it is then drawn in.
    pub fn build(
        &mut self,
        sf: &mut impl Surface,
        conversation: &Conversation,
        width: f32,
        shrink: f32,
    ) {
        let key = Key {
            epoch: sf.epoch(),
            generation: conversation.generation(),
            width: width.to_bits(),
            shrink: shrink.to_bits(),
        };
        if self.key == Some(key) {
            return;
        }
        self.key = Some(key);

        let label = paint::bound_role(sf, "list.label_role", shrink);
        let pad_x = sf.px("list.pad_x") * shrink;
        let glyph = sf.px("list.glyph") * shrink;
        let glyph_gap = sf.px("list.glyph_gap") * shrink;
        // A body row carries no chip, so it starts at the padding; a
        // headline row does, and gives up the chip's lane.
        let body_w = width - 2.0 * pad_x;
        let head_w = (body_w - glyph - glyph_gap).max(0.0);

        let mut rows = Vec::new();
        for (i, turn) in conversation.turns().iter().enumerate() {
            // One blank row between turns. It is a ROW, so what separates
            // two turns is `list.row_h` — this file has no gap of its own
            // to get wrong.
            if i > 0 {
                rows.push(RowBuf {
                    key: format!("gap{i}"),
                    ..RowBuf::default()
                });
            }
            let severity = severity(turn.severity);
            let head = paint::fit_end(sf, label.px, &turn.head, head_w, label.track);
            rows.push(RowBuf {
                key: format!("t{i}"),
                label: head,
                status: turn.status.clone(),
                severity: Some(severity),
                ..RowBuf::default()
            });
            if turn.body.is_empty() {
                continue;
            }
            for (n, line) in paint::wrap(sf, label.px, &turn.body, body_w, label.track)
                .into_iter()
                .enumerate()
            {
                rows.push(RowBuf {
                    key: format!("t{i}b{n}"),
                    label: line,
                    ..RowBuf::default()
                });
            }
        }
        self.rows = Rows::new(rows);
    }
}

/// A severity name from the closed set, or the toolkit's own fallback —
/// which `default.theme` pins to `unknown` and forbids ever being `ok`,
/// so a name this program gets wrong can never read as nominal.
fn severity(name: &str) -> Sev {
    ui::sev_of(name).unwrap_or_else(ui::sev_fallback)
}

// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paper::Paper;
    use nacelle::view::model::RowModel;
    use nacelle_ai::event::Usage;
    use nacelle_ai::message::ToolCall;

    fn text(s: &str) -> AgentEvent {
        AgentEvent::Text(s.to_string())
    }

    fn started() -> AgentEvent {
        AgentEvent::TurnStarted {
            turn: 1,
            model: "test-model".into(),
        }
    }

    fn labels(cache: &RowCache) -> Vec<String> {
        let mut out = Vec::new();
        let mut buf = RowBuf::new();
        for i in 0..cache.rows().len() {
            cache.rows().row(i, &mut buf);
            out.push(buf.label.clone());
        }
        out
    }

    #[test]
    fn a_run_of_fragments_is_one_turn_and_not_one_turn_per_packet() {
        let mut c = Conversation::new();
        c.asked("hello");
        c.apply(&started());
        c.apply(&text("the "));
        c.apply(&text("quick "));
        c.apply(&text("fox"));
        assert_eq!(c.turns().len(), 2, "the question and one answer");
        assert_eq!(c.turns()[1].voice, Voice::Model);
        assert_eq!(c.turns()[1].body, "the quick fox");
    }

    #[test]
    fn a_tool_between_two_fragments_ends_the_paragraph_it_interrupted() {
        let mut c = Conversation::new();
        c.asked("set the theme");
        c.apply(&started());
        c.apply(&text("looking"));
        c.apply(&AgentEvent::ToolStarted {
            call: ToolCall {
                id: "1".into(),
                name: "nacelle_list_themes".into(),
                input: serde_json::json!({}),
            },
        });
        c.apply(&text("found it"));
        let bodies: Vec<&str> = c.turns().iter().map(|t| t.body.as_str()).collect();
        assert_eq!(bodies[1], "looking");
        assert_eq!(bodies[3], "found it", "the tool did not swallow the answer");
        assert_eq!(c.turns()[3].voice, Voice::Model);
    }

    #[test]
    fn several_model_turns_in_one_exchange_stay_one_answer() {
        // The loop emits TurnStarted for every model turn, tool rounds
        // included; the reader asked once and is owed one answer.
        let mut c = Conversation::new();
        c.asked("go");
        c.apply(&started());
        c.apply(&text("first "));
        c.apply(&AgentEvent::TurnStarted {
            turn: 2,
            model: "test-model".into(),
        });
        c.apply(&text("second"));
        assert_eq!(c.turns().len(), 2);
        assert_eq!(c.turns()[1].body, "first second");
    }

    #[test]
    fn the_headline_names_the_model_the_provider_actually_used() {
        let mut c = Conversation::new();
        c.asked("go");
        c.apply(&AgentEvent::TurnStarted {
            turn: 1,
            model: "claude-haiku-4-5".into(),
        });
        c.apply(&text("hi"));
        assert_eq!(c.turns()[1].head, "CLAUDE-HAIKU-4-5");
        assert_eq!(c.answering(), Some("claude-haiku-4-5"));
    }

    #[test]
    fn every_state_the_window_must_show_says_what_to_do_about_it() {
        let cases = [
            Notice::NoOllama {
                detail: "nothing is listening".into(),
            },
            Notice::NoCredential {
                detail: "no token".into(),
            },
            Notice::NoBackend,
            Notice::AuthRejected {
                detail: "401".into(),
            },
            Notice::Refused {
                detail: "declined".into(),
            },
            Notice::Cancelled,
            Notice::TurnLimit { limit: 16 },
        ];
        for notice in cases {
            let severity = notice.severity();
            assert!(
                ui::sev_of(severity).is_some(),
                "{severity} is not in the theme's closed severity set"
            );
            assert_ne!(severity, "ok", "a state worth showing is never nominal");
            let mut c = Conversation::new();
            c.note(notice.clone());
            let turn = &c.turns()[0];
            assert!(!turn.head.is_empty(), "{notice:?} has no headline");
            assert!(
                turn.body.split_whitespace().count() >= 8,
                "{notice:?} does not say what to do about it"
            );
        }
    }

    #[test]
    fn a_cancelled_exchange_says_so_and_stops_being_busy() {
        let mut c = Conversation::new();
        c.asked("go");
        c.apply(&started());
        c.apply(&text("half a sen"));
        assert!(c.is_busy());
        c.apply(&AgentEvent::Failed(AgentError::Cancelled));
        assert!(!c.is_busy());
        assert_eq!(c.turns().last().unwrap().voice, Voice::Notice);
        assert_eq!(c.turns().last().unwrap().severity, "info");
        assert_eq!(
            c.turns()[1].body,
            "half a sen",
            "what had arrived is kept, not thrown away"
        );
    }

    #[test]
    fn the_turn_ceiling_is_reported_with_the_number_it_stopped_at() {
        let mut c = Conversation::new();
        c.asked("go");
        c.apply(&AgentEvent::Failed(AgentError::TurnLimit { limit: 16 }));
        assert!(c.turns().last().unwrap().body.contains("16"));
        assert_eq!(c.turns().last().unwrap().severity, "warning");
    }

    #[test]
    fn a_refusal_is_told_apart_from_a_failure() {
        let mut refused = Conversation::new();
        refused.apply(&AgentEvent::Failed(AgentError::Backend(
            BackendError::Refused {
                category: Some("policy".into()),
                explanation: None,
            },
        )));
        assert_eq!(refused.turns()[0].severity, "contained");

        let mut broken = Conversation::new();
        broken.apply(&AgentEvent::Failed(AgentError::Backend(
            BackendError::Network("reset".into()),
        )));
        assert_eq!(broken.turns()[0].severity, "critical");
    }

    #[test]
    fn a_finished_exchange_puts_its_cost_on_the_answer() {
        let mut c = Conversation::new();
        c.asked("go");
        c.apply(&started());
        c.apply(&text("done"));
        c.apply(&AgentEvent::Finished(Completion {
            text: "done".into(),
            turns: 1,
            tools_run: 0,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 3,
                cache_read_tokens: 5,
                cache_write_tokens: 0,
            },
            stop: StopReason::EndTurn,
        }));
        assert_eq!(c.turns()[1].status, "15 in · 3 out");
        assert!(!c.is_busy());
    }

    #[test]
    fn a_reply_cut_off_by_the_token_ceiling_says_it_is_a_fragment() {
        let mut c = Conversation::new();
        c.asked("go");
        c.apply(&started());
        c.apply(&text("half"));
        c.apply(&AgentEvent::Finished(Completion {
            text: "half".into(),
            turns: 1,
            tools_run: 0,
            usage: Usage::default(),
            stop: StopReason::MaxTokens,
        }));
        assert_eq!(c.turns().last().unwrap().voice, Voice::Notice);
        assert!(c.turns().last().unwrap().body.contains("fragment"));
    }

    #[test]
    fn asking_pins_the_view_to_the_newest_row_again() {
        let mut c = Conversation::new();
        c.set_follows_tail(false);
        assert!(!c.follows_tail());
        c.asked("go");
        assert!(c.follows_tail(), "a question is sent from the bottom");
        // Reading back through a long answer must not be interrupted by
        // the answer still arriving.
        c.set_follows_tail(false);
        c.apply(&started());
        c.apply(&text("more"));
        assert!(!c.follows_tail());
    }

    #[test]
    fn every_change_moves_the_generation_the_wrap_is_cached_on() {
        let mut c = Conversation::new();
        let g0 = c.generation();
        c.asked("go");
        let g1 = c.generation();
        assert_ne!(g0, g1);
        c.apply(&text("a"));
        assert_ne!(g1, c.generation());
    }

    #[test]
    fn a_turn_becomes_a_headline_row_and_one_row_per_wrapped_line() {
        let mut paper = Paper::new();
        let mut c = Conversation::new();
        c.asked("one two three four");
        let mut cache = RowCache::new();
        // A box that can hold about half the sentence, measured rather
        // than guessed: the type role's size comes from the theme, so a
        // pixel width written here would be a number that goes stale the
        // day the master changes `type.body.size`.
        let role = paint::bound_role(&mut paper, "list.label_role", 1.0);
        let whole = paper.measure(role.px, "one two three four", role.track);
        let pad = paper.px("list.pad_x");
        cache.build(&mut paper, &c, whole / 2.0 + 2.0 * pad, 1.0);
        let labels = labels(&cache);
        assert_eq!(labels[0], "YOU");
        assert!(labels.len() > 2, "the body wrapped: {labels:?}");
        assert_eq!(
            labels[1..].join(" "),
            "one two three four",
            "wrapping loses no words and invents none"
        );
    }

    #[test]
    fn turns_are_separated_by_a_row_and_never_by_a_number_of_our_own() {
        let mut paper = Paper::new();
        let mut c = Conversation::new();
        c.asked("a");
        c.apply(&started());
        c.apply(&text("b"));
        let mut cache = RowCache::new();
        cache.build(&mut paper, &c, 4000.0, 1.0);
        let labels = labels(&cache);
        assert_eq!(labels, vec!["YOU", "a", "", "TEST-MODEL", "b"]);
    }

    #[test]
    fn only_a_headline_row_carries_the_chip_that_says_whose_turn_it_is() {
        let mut paper = Paper::new();
        let mut c = Conversation::new();
        c.asked("a");
        c.apply(&started());
        c.apply(&text("b"));
        let mut cache = RowCache::new();
        cache.build(&mut paper, &c, 4000.0, 1.0);
        let mut buf = RowBuf::new();
        let mut chips = Vec::new();
        for i in 0..cache.rows().len() {
            cache.rows().row(i, &mut buf);
            chips.push(buf.severity);
        }
        assert_eq!(chips[0], Some(severity(SEV_YOU)));
        assert_eq!(chips[1], None);
        assert_eq!(chips[3], Some(severity(SEV_MODEL)));
        assert_ne!(
            chips[0], chips[3],
            "the question and the answer must not read alike"
        );
    }

    #[test]
    fn the_wrap_is_rebuilt_when_the_text_grows_and_not_otherwise() {
        let mut paper = Paper::new();
        let mut c = Conversation::new();
        c.asked("a");
        let mut cache = RowCache::new();
        cache.build(&mut paper, &c, 1000.0, 1.0);
        let before = paper.measurements();
        cache.build(&mut paper, &c, 1000.0, 1.0);
        assert_eq!(paper.measurements(), before, "nothing changed, nothing re-measured");
        c.apply(&text("b"));
        cache.build(&mut paper, &c, 1000.0, 1.0);
        assert!(paper.measurements() > before);
        // A narrower window is a different wrap even for the same text.
        let mid = paper.measurements();
        cache.build(&mut paper, &c, 500.0, 1.0);
        assert!(paper.measurements() > mid);
    }
}
