//! Layer 3: the local model's opinion, and the narrow thing it is
//! allowed to do with it.
//!
//! Patterns catch structure. They cannot catch meaning, and meaning is
//! where the rest of what a person would not want sent lives: a
//! diagnosis in a note, an unannounced product in a plan, a message from
//! somebody who was not writing to a model. This layer is where that is
//! looked for, and it is genuinely useful — it is simply not first.
//!
//! **It can only remove.** The whole API is a list of quotes to take
//! out. There is no way to express "keep this", no way to return an
//! edited payload, and therefore no sequence of model outputs that puts
//! back what layers 1 and 2 took. That is a property of the types rather
//! than a rule somebody has to remember, which is the only kind of
//! property worth having here: a reviewer that decides the marker was a
//! mistake can delete the marker, and deleting a marker does not produce
//! a key.
//!
//! **It runs on the local machine.** Sending the payload somewhere to
//! ask whether it may be sent is the joke this design cannot afford, so
//! [`LocalReviewer`] refuses any backend that is not local — see
//! [`Backend::is_local`](crate::backend::Backend::is_local).
//!
//! **Its own words are scanned before they are used.** A reviewer
//! explains what it removed and why, and a model quoting the secret back
//! in that explanation would put it straight into the payload it was
//! asked to clean. The reason goes through
//! [`scan`](super::scan) before it reaches the marker.

use std::fmt;

use crate::backend::{Backend, Flow};
use crate::event::StreamEvent;
use crate::message::{Message, Request};

use super::scan;

/// How long an explanation may be once it is in the payload. Long enough
/// for a sentence, short enough that a model which decided to reply with
/// the file cannot spend the payload on it.
const MAX_WHY: usize = 160;

/// How many quotes are honoured from one review. A reviewer with more
/// than this to say about one payload is not reviewing it.
const MAX_REMOVALS: usize = 64;

/// What the local model wants taken out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Removal {
    /// The exact text to remove. Every occurrence of it goes.
    pub quote: String,
    /// Why, in the reviewer's own words — shown to the user and put in
    /// the marker, after it has been scanned like anything else.
    pub why: String,
}

impl Removal {
    pub fn new(quote: impl Into<String>, why: impl Into<String>) -> Self {
        Removal {
            quote: quote.into(),
            why: why.into(),
        }
    }
}

/// One review.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Review {
    pub removals: Vec<Removal>,
    /// Why the review is worth less than it looks: the model was not
    /// reachable, its answer did not parse, it was not asked at all.
    /// Carried rather than hidden, because the manifest has to be able
    /// to tell the user that this layer did not run.
    pub note: Option<String>,
}

impl Review {
    pub fn nothing() -> Self {
        Review::default()
    }

    /// A review that did not happen, and says so.
    pub fn failed(note: impl Into<String>) -> Self {
        Review {
            removals: Vec::new(),
            note: Some(note.into()),
        }
    }
}

/// Whoever gives the opinion.
///
/// A trait rather than a concrete type because the useful
/// implementations are a local model, nothing at all, and — in tests —
/// something that returns exactly what the test needs to prove a
/// property about.
pub trait Reviewer {
    fn review(&mut self, payload: &str) -> Review;
}

/// No opinion. The right reviewer when no local model is loaded: layers
/// 1 and 2 have already run, and an absent layer 3 is a smaller loss
/// than a fabricated one.
pub struct NoReview;

impl Reviewer for NoReview {
    fn review(&mut self, _payload: &str) -> Review {
        Review::nothing()
    }
}

/// Take out everything a review asked for.
///
/// Returns the text and what it cost. Quotes that are not in the text
/// are ignored rather than reported as an error: a model paraphrasing
/// what it wanted removed is a bad review, not a reason to abandon a
/// payload that two deterministic layers have already cleaned.
pub fn apply(text: &str, review: &Review) -> (String, Vec<Removal>) {
    let mut out = text.to_string();
    let mut done = Vec::new();

    for removal in review.removals.iter().take(MAX_REMOVALS) {
        let quote = removal.quote.trim();
        // An empty quote matches between every pair of characters, and
        // a one-character quote is not a judgement about meaning.
        if quote.len() < 2 || !out.contains(quote) {
            continue;
        }
        let why = short(&scan::scan(&removal.why).text);
        let marker = format!("[[redacted: {why} — removed by the local model's review]]");
        out = out.replace(quote, &marker);
        done.push(Removal {
            quote: quote.to_string(),
            why,
        });
    }

    (out, done)
}

/// Trim an explanation to one line of sensible length.
fn short(why: &str) -> String {
    let why = why.trim().replace(['\n', '\r'], " ");
    let why = why.trim();
    if why.is_empty() {
        return "the local model judged this sensitive".to_string();
    }
    if why.len() <= MAX_WHY {
        return why.to_string();
    }
    let mut cut = MAX_WHY;
    while cut > 0 && !why.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &why[..cut])
}

/// A backend that is not on this machine was offered as the reviewer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotLocal(pub String);

impl fmt::Display for NotLocal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "\"{}\" is not a local backend: asking a remote model whether a payload \
             may be sent would send it",
            self.0
        )
    }
}

impl std::error::Error for NotLocal {}

/// What the reviewing model is told. Deliberately narrow: it is not
/// being asked to summarise, to be helpful, or to decide whether the
/// escalation is a good idea.
const SYSTEM: &str = "\
You are the last check before a piece of text leaves this computer for a remote model. \
Structural secrets — keys, tokens, passwords — have already been removed by pattern \
matching, and where one was removed you will see a [[redacted: ...]] marker.
Your job is the part matching cannot do: private MEANING. A person's health, a private \
message, an unreleased plan, an address, anything a reasonable owner of this machine \
would not want a third party to hold.
Answer with JSON and nothing else, in this shape:
{\"remove\": [{\"quote\": \"the exact text to remove\", \"why\": \"one short sentence\"}]}
Rules you cannot break: quote text EXACTLY as it appears; never quote a secret value in \
\"why\"; if nothing needs removing answer {\"remove\": []}. You cannot put anything back \
— asking to keep something is not an option this format has.";

/// The local model as the reviewer.
///
/// Holds its own backend rather than borrowing the agent's: the review
/// happens between the agent deciding to escalate and the payload being
/// sent, and a backend that was mid-turn cannot answer a second question
/// — one turn at a time is the contract every backend is written to.
pub struct LocalReviewer {
    backend: Box<dyn Backend>,
    model: String,
    max_tokens: u32,
}

impl LocalReviewer {
    /// A reviewer over a local backend, or a refusal to build one.
    pub fn new(backend: Box<dyn Backend>, model: impl Into<String>) -> Result<Self, NotLocal> {
        if !backend.is_local() {
            return Err(NotLocal(backend.name().to_string()));
        }
        Ok(LocalReviewer {
            backend,
            model: model.into(),
            // A review is a short list, and a model that decided to
            // write an essay instead should be cut off rather than
            // waited for.
            max_tokens: 1024,
        })
    }
}

/// Written out rather than derived, because a `Box<dyn Backend>` has no
/// `Debug` and should not grow one: what identifies a backend is its
/// name, and everything else it holds is a connection pool or a
/// credential.
impl fmt::Debug for LocalReviewer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalReviewer")
            .field("backend", &self.backend.name())
            .field("model", &self.model)
            .finish()
    }
}

impl Reviewer for LocalReviewer {
    fn review(&mut self, payload: &str) -> Review {
        let request = Request::new(self.model.clone())
            .with_system(SYSTEM.to_string())
            .with_message(Message::user(payload))
            .with_max_tokens(self.max_tokens);

        let mut answer = String::new();
        let sent = self.backend.send(&request, &mut |event| {
            if let StreamEvent::Text(text) = event {
                answer.push_str(&text);
            }
            Flow::Continue
        });

        // A review that could not be had is reported as one that could
        // not be had. Inventing an empty result would tell the manifest
        // that layer 3 passed, which is a different and untrue thing.
        if let Err(err) = sent {
            return Review::failed(format!("the local model could not review this: {err}"));
        }
        parse(&answer)
    }
}

/// The model's answer, as removals.
///
/// Models put JSON in prose and in code fences whatever they are told,
/// so the object is looked for rather than demanded — but only the
/// object: anything outside it is ignored, never treated as an
/// instruction.
pub fn parse(answer: &str) -> Review {
    let Some(document) = json_object(answer) else {
        return Review::failed("the local model's review was not JSON and was ignored");
    };
    let Some(list) = document.get("remove").and_then(|v| v.as_array()) else {
        return Review::failed("the local model's review had no \"remove\" list and was ignored");
    };

    let mut removals = Vec::new();
    for item in list {
        let Some(quote) = item.get("quote").and_then(|v| v.as_str()) else {
            continue;
        };
        let why = item
            .get("why")
            .and_then(|v| v.as_str())
            .unwrap_or("the local model judged this sensitive");
        removals.push(Removal::new(quote, why));
    }
    Review {
        removals,
        note: None,
    }
}

/// The first balanced `{...}` in the text, parsed.
fn json_object(text: &str) -> Option<serde_json::Value> {
    let bytes = text.as_bytes();
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, byte) in bytes.iter().enumerate().skip(start) {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&text[start..=offset]).ok();
                }
            }
            _ => {}
        }
    }
    None
}
