//! Keeping what is private on this machine, in four layers.
//!
//! | layer | module | what it is |
//! |---|---|---|
//! | 1 | [`deny`] | files that are never opened |
//! | 2 | [`scan`] | credential shapes cut out of anything about to be sent |
//! | 3 | [`review`] | the local model's opinion, which may only remove more |
//! | 4 | [`manifest`] | what the user is shown before it goes |
//!
//! The order is the point. The obvious implementation of "do not send
//! secrets" is to show the text to a model and ask, and it is the
//! weakest design available: a model's judgement is a probability, not a
//! boundary, and the cases it gets wrong are exactly the cases that
//! matter — a key pasted into a log, a token in a URL, a password in a
//! shell history line. One miss is on somebody else's server and cannot
//! be recalled.
//!
//! So the model is the LAST layer, not the first, and each layer is
//! strictly weaker than the one before it:
//!
//! 1. Never read it. Nothing downstream can leak bytes that were never
//!    in memory.
//! 2. Never send it. Deterministic matching on shapes, identical on
//!    every payload, unaffected by anything the model believes.
//! 3. Then ask the model — about meaning, which the first two cannot
//!    see — and let it remove more. Never less.
//! 4. Then tell the user, and let them answer.
//!
//! ## Two states, because an order is not a comment
//!
//! That pipeline is two types rather than one, and the split is the
//! whole reason this module reads the way it does.
//!
//! [`Gathering`] is a payload being put together. Text goes in and is
//! scanned on the way in; there is no constructor that takes text
//! already considered clean, so layer 2 cannot be skipped by a caller in
//! a hurry.
//!
//! [`Outgoing`] is a payload layer 3 has finished with. It has no method
//! that adds anything, and the only way to get one is
//! [`Gathering::reviewed`] or [`Gathering::unreviewed`] — one way, no
//! way back. That is what stops the failure this shape exists to
//! prevent: a payload marked "the local model has seen this" which then
//! grows by a paragraph nothing has seen. It used to be a `bool`, and a
//! `bool` cannot refuse.
//!
//! The two examples below are a pair, and the first one is not
//! decoration: a `compile_fail` block passes when the code fails to
//! build *for any reason at all*, including a typo. The first is
//! character-for-character the second without its last line, so the only
//! thing the second can be failing on is the line that adds to a payload
//! layer 3 has finished with.
//!
//! ```
//! use nacelle_ai::{Gathering, NoReview};
//!
//! let outgoing = Gathering::new()
//!     .with_text("why does the desktop start with no panels?")
//!     .reviewed(&mut NoReview);
//! assert!(outgoing.payload().contains("no panels"));
//! ```
//!
//! ```compile_fail
//! use nacelle_ai::{Gathering, NoReview};
//!
//! let outgoing = Gathering::new()
//!     .with_text("why does the desktop start with no panels?")
//!     .reviewed(&mut NoReview);
//! assert!(outgoing.payload().contains("no panels"));
//! // `Outgoing` has no `with_text`: this does not compile, which is
//! // the whole mechanism. A payload cannot grow after the last layer
//! // that looked at it, so there is no state in which "reviewed" and
//! // "there is text in here nothing reviewed" are both true.
//! let outgoing = outgoing.with_text("and here is the log, unexamined");
//! ```
//!
//! An [`Outgoing`] is still not permission to send. Its
//! [`payload`](Outgoing::payload) is text the first three layers have
//! already been through, which is why looking at it is safe — but what
//! makes it *sendable* is the user's answer, and that lives in
//! [`supervise::handoff`](crate::supervise::handoff). Nothing here
//! reaches a network, and nothing here decides that anything may.
//!
//! ## Where the numbers on a manifest come from
//!
//! They are read off the payload itself, at the moment it is finished,
//! and never kept alongside it. A payload is held as its pieces until
//! [`Gathering::reviewed`] joins them, so "how many bytes of this file
//! are in it" is the length of that file's piece *after* layer 3 has
//! taken what it wanted — not a length written down before the removal
//! happened and never revisited. A manifest whose figures are a separate
//! tally is a manifest that is right until the day the two disagree, and
//! there is no worse thing for this layer to be.

pub mod deny;
pub mod manifest;
pub mod review;
pub mod scan;

use std::path::PathBuf;

pub use deny::{Denial, Denylist, Reason};
pub use manifest::{Disclosure, Manifest, Why};
pub use review::{apply, LocalReviewer, NoReview, NotLocal, Removal, Review, Reviewer};
pub use scan::{marker, scan, Finding, Kind, Redacted};

/// One file whose contents are in a payload.
///
/// The counts are what makes a manifest add up: how much was read, how
/// much survived redaction, and how many things were taken out of it.
/// None of them is a secret — the length of a key is not the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub path: PathBuf,
    pub bytes_read: usize,
    pub bytes_sent: usize,
    pub removed: usize,
}

/// The note appended to a payload that had something taken out of it.
///
/// Without this the far model reads a marker as noise. With it, the
/// marker is a fact it can act on: something was withheld, and the
/// person who can supply it is sitting in front of the machine.
///
/// Crate-visible because a payload is not always one string: the
/// request path in [`supervise::seal`](crate::supervise::seal) keeps
/// its pieces apart and has to put this note somewhere the endpoint
/// accepts. The words are the same wherever it ends up.
pub(crate) const WITHHELD: &str = "\
[[note: the local agent removed one or more values from this message before sending it — \
they are marked [[redacted: ...]] above. If one of them is what you need in order to \
answer, say so and ask the user for it. Do not guess what was there, and do not answer \
as though nothing was missing.]]";

/// The file a piece of a payload was read from.
#[derive(Clone, Debug)]
struct Origin {
    path: PathBuf,
    /// The file's size on disk. The only number here that is not read
    /// back off the payload, because it is the one thing the payload
    /// cannot know: what did not survive layer 2 is not in it.
    bytes_read: usize,
}

/// One piece of a payload being gathered.
///
/// The pieces are kept apart until the payload is finished, and that is
/// what lets a manifest say something true about one file: layer 3 edits
/// a piece in place, so the bytes a file contributes are the length of
/// its piece when everything has run, not a figure recorded before the
/// last layer touched it.
#[derive(Clone, Debug)]
struct Piece {
    /// The line naming the file, when this piece is one. Held apart from
    /// the contents so that "bytes of this file in the payload" is not
    /// "bytes of this file plus a header this program wrote".
    label: Option<String>,
    text: String,
    origin: Option<Origin>,
    /// How many things have been taken out of this piece, by any layer.
    removed: usize,
}

/// A payload being put together, and the only state in which anything
/// can be added to one.
///
/// Every piece of text is scanned as it goes in, so there is no window
/// in which this holds unredacted content. The pieces stay in the order
/// they were added, since a payload is a message and a message that
/// arrives shuffled is a different message.
///
/// It becomes an [`Outgoing`] through [`Gathering::reviewed`] or
/// [`Gathering::unreviewed`], and there is no way back — see the module
/// header for why that is a type rather than a rule.
#[derive(Clone, Debug, Default)]
pub struct Gathering {
    pieces: Vec<Piece>,
    findings: Vec<Finding>,
}

impl Gathering {
    pub fn new() -> Self {
        Gathering::default()
    }

    /// Add prose — the user's question, the agent's own summary, a tool
    /// result. Scanned on the way in like everything else: a tool result
    /// is a file's contents with a wrapper around it.
    pub fn with_text(mut self, text: &str) -> Self {
        let Redacted { text, findings } = scan::scan(text);
        self.pieces.push(Piece {
            label: None,
            text,
            origin: None,
            removed: findings.len(),
        });
        self.findings.extend(findings);
        self
    }

    /// Read a file through layer 1 and add it.
    ///
    /// The way to put a file in a payload. Layers 1 and 2 happen in one
    /// call, in that order, and a file on the denylist never reaches the
    /// second one — the refusal comes back instead, for the user to be
    /// told about.
    pub fn read_file(
        self,
        guard: &deny::Denylist,
        path: impl Into<PathBuf>,
    ) -> Result<Self, crate::tools::error::ToolError> {
        let path = path.into();
        let contents = guard.read_to_string(&path)?;
        Ok(self.with_file(path, &contents))
    }

    /// Add the contents of a file the caller already holds.
    ///
    /// For content that was not read here: a tool result, something the
    /// user pasted, a file already on screen. Whatever produced it is
    /// responsible for having gone through layer 1 —
    /// [`Gathering::read_file`] is the version that cannot forget.
    ///
    /// The path travels with the contents. It is part of what leaves the
    /// machine and the manifest lists it, so the user sees the path
    /// before deciding — but a file's name is often the only thing that
    /// makes its contents intelligible, and stripping it would make the
    /// remote model guess.
    pub fn with_file(mut self, path: impl Into<PathBuf>, contents: &str) -> Self {
        let path = path.into();
        let Redacted { text, findings } = scan::scan(contents);
        self.pieces.push(Piece {
            label: Some(format!("--- {} ---", path.display())),
            text,
            origin: Some(Origin {
                path,
                bytes_read: contents.len(),
            }),
            removed: findings.len(),
        });
        self.findings.extend(findings);
        self
    }

    /// Layer 3: let the local model take out more, and finish.
    ///
    /// Consumes the gathering and hands back something that cannot be
    /// added to. That is the ordering, as a type: a payload the model
    /// has seen cannot afterwards grow by a paragraph it has not.
    pub fn reviewed(self, reviewer: &mut dyn Reviewer) -> Outgoing {
        // The model reads the payload as one piece of text, because
        // meaning does not stop at a piece boundary. What it asks for is
        // taken out of the pieces one at a time below, so what is
        // reported is what actually came out.
        let review = reviewer.review(&joined(&self.pieces));
        self.finish(Some(review))
    }

    /// Finish without layer 3, because there is no local model to ask.
    ///
    /// Honest rather than convenient: the manifest this produces says
    /// outright that no model reviewed the payload, since an absent
    /// layer 3 and a layer 3 that found nothing are not the same
    /// assurance and must not look alike on the screen the user answers.
    pub fn unreviewed(self) -> Outgoing {
        self.finish(None)
    }

    fn finish(mut self, review: Option<Review>) -> Outgoing {
        let mut removals: Vec<Removal> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let reviewed = review.is_some();

        if let Some(review) = review {
            if let Some(note) = review.note.clone() {
                notes.push(note);
            }
            for piece in &mut self.pieces {
                // The label is a path, and a path is about to be sent
                // like everything else here, so a reviewer that judges
                // one private is answered rather than overruled.
                if let Some(label) = &mut piece.label {
                    let (out, done) = review::apply(label, &review);
                    *label = out;
                    piece.removed += done.len();
                    remember(&mut removals, done);
                }
                let (out, done) = review::apply(&piece.text, &review);
                piece.text = out;
                piece.removed += done.len();
                remember(&mut removals, done);
            }
        }

        // Read off the finished pieces, never accumulated alongside
        // them: this is the only place these numbers exist, so there is
        // no second one to disagree with.
        let sources = self
            .pieces
            .iter()
            .filter_map(|piece| {
                piece.origin.as_ref().map(|origin| Source {
                    path: origin.path.clone(),
                    bytes_read: origin.bytes_read,
                    bytes_sent: piece.text.len(),
                    removed: piece.removed,
                })
            })
            .collect();

        let mut body = joined(&self.pieces);
        if !self.findings.is_empty() || !removals.is_empty() {
            say_something_was_withheld(&mut body);
        }

        Outgoing {
            body,
            sources,
            findings: self.findings,
            removals,
            reviewed,
            notes,
        }
    }
}

/// What is about to cross the network, and everything known about it.
///
/// Layers 2 and 3 have run and nothing more can be put in: the type has
/// no method that adds text, and its fields are private. What it holds
/// is the finished payload — one string, the one the manifest is
/// measured against and the one a [`Cleared`](crate::Cleared) carries —
/// so there is nothing here that can be computed twice and come out
/// differently the second time.
#[derive(Clone, Debug)]
pub struct Outgoing {
    body: String,
    sources: Vec<Source>,
    findings: Vec<Finding>,
    removals: Vec<Removal>,
    reviewed: bool,
    notes: Vec<String>,
}

impl Outgoing {
    /// A payload that was redacted somewhere else, with the account of
    /// what came out of it.
    ///
    /// The one caller is [`supervise::seal`](crate::supervise::seal),
    /// and it cannot use [`Gathering`]: a request is a *structure* — a
    /// system prompt, messages, tool calls with their results — and what
    /// the endpoint accepts is that structure, not one string. So it
    /// runs layers 2 and 3 over every piece in place and hands the
    /// finished text here, where the manifest is built.
    ///
    /// `body` is that text as the request carries it, read back off the
    /// request after every layer has run. It is not re-assembled here
    /// and nothing is appended to it — including the
    /// [`WITHHELD`] note, which the seal puts in the request's own
    /// system prompt where the endpoint will accept it. Anything this
    /// function added would be a byte on the manifest that is not a byte
    /// on the wire.
    ///
    /// Crate-private, and it stays that way. Outside this crate the only
    /// way text gets into a payload is [`Gathering::with_text`] and
    /// [`Gathering::read_file`], both of which scan — which is the
    /// promise this module's header makes, and it is only a promise
    /// while there is no public constructor that takes text somebody has
    /// already decided is clean.
    pub(crate) fn from_parts(
        body: String,
        sources: Vec<Source>,
        findings: Vec<Finding>,
        removals: Vec<Removal>,
        reviewed: bool,
        mut notes: Vec<String>,
    ) -> Self {
        // Said outright, because the alternative is a manifest that
        // reads "Files: none" over a payload whose tool results are
        // somebody's config file, and an empty list read as "no files"
        // is the manifest telling the user something that may not be
        // true.
        //
        // The list used to be empty always — this constructor hard-coded
        // it — which had a second consequence nobody had measured:
        // `Disclosure::required_for` asks about unseen files, so with no
        // files there was never anything new, and after one yes layer 4
        // was never asked again for the rest of the session. Measured: a
        // second turn carrying a diary the user had never seen a word of
        // went with no manifest at all.
        //
        // The seal now names the files it can name — a result whose call
        // carried a declared path argument — and this note says what is
        // still missing rather than pretending the list is complete.
        notes.push(
            "this list names the files a tool was asked for by path; anything the model \
             quoted, pasted or summarised into the conversation is in what is being sent \
             and is not on it"
                .to_string(),
        );

        Outgoing {
            body,
            sources,
            findings,
            removals,
            reviewed,
            notes,
        }
    }

    /// The text to send: every piece that was added, in order, plus the
    /// note that says something was withheld when something was.
    ///
    /// A borrow rather than a fresh `String`, and deliberately: this is
    /// the payload, not a rendering of it. There is one of it, the
    /// manifest's byte count is its length, and a
    /// [`Cleared`](crate::Cleared) carries a copy of this exact string.
    pub fn payload(&self) -> &str {
        &self.body
    }

    /// How many bytes of the user's text would leave the machine.
    pub fn bytes(&self) -> usize {
        self.body.len()
    }

    pub fn sources(&self) -> &[Source] {
        &self.sources
    }

    /// What layer 2 took out.
    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    /// What layer 3 took out.
    pub fn removals(&self) -> &[Removal] {
        &self.removals
    }

    /// Whether anything was taken out at all.
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty() && self.removals.is_empty()
    }

    /// Whether a local model read this payload. `false` is a fact the
    /// manifest states rather than hides.
    pub fn was_reviewed(&self) -> bool {
        self.reviewed
    }

    /// Layer 4: what the user is shown.
    ///
    /// `destination` is the backend's name and `reason` is why the local
    /// agent wants to escalate — both go on the screen the user answers,
    /// so both are written for them rather than for a log.
    pub fn manifest(&self, destination: &str, reason: &str, why: Why) -> Manifest {
        let mut removed: Vec<(String, usize)> = Vec::new();
        for what in self
            .findings
            .iter()
            .map(|finding| finding.kind.what())
            .chain(self.removals.iter().map(|r| r.why.clone()))
        {
            match removed.iter_mut().find(|(known, _)| *known == what) {
                Some((_, count)) => *count += 1,
                None => removed.push((what, 1)),
            }
        }

        let mut notes = self.notes.clone();
        if !self.reviewed {
            // Said outright: an absent layer 3 and a layer 3 that found
            // nothing look identical on a manifest that does not
            // distinguish them, and they are not the same assurance.
            notes.push(
                "no local model reviewed this for private meaning — only the pattern rules ran"
                    .to_string(),
            );
        }

        Manifest {
            destination: destination.to_string(),
            reason: reason.to_string(),
            why,
            sources: self.sources.clone(),
            bytes: self.bytes(),
            removed,
            notes,
        }
    }
}

/// The pieces as one payload.
///
/// Pieces are separated by a blank line and a file's piece is preceded
/// by the line naming it. This is the only place a payload is assembled,
/// so it is the only place its size is decided.
fn joined(pieces: &[Piece]) -> String {
    let mut out = String::new();
    for piece in pieces {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        if let Some(label) = &piece.label {
            out.push_str(label);
            out.push('\n');
        }
        out.push_str(&piece.text);
    }
    out
}

/// Tell the far model that it is reading a payload with holes in it.
fn say_something_was_withheld(body: &mut String) {
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body.push('\n');
    body.push_str(WITHHELD);
}

/// Record what a review actually took out, without counting a quote that
/// occurred in two pieces as two removals.
fn remember(known: &mut Vec<Removal>, done: Vec<Removal>) {
    for removal in done {
        if !known.contains(&removal) {
            known.push(removal);
        }
    }
}
