//! Layer 4: what the user is shown before anything leaves.
//!
//! The three layers underneath are mechanisms the user cannot observe.
//! They either work or they do not, and from the outside those look
//! identical right up until the day they do not. This layer is what
//! makes the other three auditable: before the first escalation of a
//! session, and again whenever the payload carries file content the user
//! has not already seen in the conversation, they are shown **what is
//! about to leave the machine** — which files, how many bytes, and what
//! was cut out of them. Then they answer.
//!
//! Two decisions worth stating, because both could plausibly have gone
//! the other way:
//!
//! **It is not shown every time.** A manifest before every single
//! escalation would be read once, skimmed twice and clicked through for
//! ever after, which is a worse outcome than no manifest at all: it
//! trains the exact reflex it exists to interrupt. So it is shown when
//! there is something new to see, and [`Disclosure`] is the thing that
//! remembers what is not new.
//!
//! **It never prints what it is describing.** The manifest lists paths,
//! sizes and the *kinds* of thing that were removed. A manifest that
//! quoted the payload would be one more copy of the payload, on screen,
//! in a window that may be shared.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use super::Source;

/// Why the user is being shown this now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// Nothing has left this machine yet in this session.
    FirstEscalation,
    /// The payload carries the contents of a file the user has not seen
    /// in this conversation.
    UnseenFile,
}

impl Why {
    pub fn sentence(&self) -> &'static str {
        match self {
            Why::FirstEscalation => "this is the first thing this session would send off the machine",
            Why::UnseenFile => {
                "this carries the contents of a file you have not seen in this conversation"
            }
        }
    }
}

/// What is about to leave, in the words the user answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// Who would receive it — the backend's name, never a URL and never
    /// anything derived from a credential.
    pub destination: String,
    /// Why the local agent wants to escalate, in one line.
    pub reason: String,
    pub why: Why,
    /// The files whose contents are in the payload.
    pub sources: Vec<Source>,
    /// How big the payload is after redaction: the user's own text as
    /// it will leave, not what was read off the disk and not the wire
    /// format's envelope around it.
    ///
    /// Read off the finished payload — the same string a
    /// [`Cleared`](crate::Cleared) carries, or the same walk over the
    /// same request a
    /// [`Sealed`](crate::Sealed) will be encoded from — rather than
    /// accumulated as the layers run. A count kept alongside a payload
    /// is a second account of it, and the day the two disagree is the
    /// day this number is a lie.
    ///
    /// It is never smaller than what goes. It can be larger in exactly
    /// one case, which is named where it arises: a thinking block with
    /// no signature is counted here because the seal walks the request,
    /// and dropped by the Anthropic encoder because an unsigned block
    /// cannot be replayed. Nothing reaches a socket that this number did
    /// not include.
    ///
    /// That sentence was once false, and it was false in the dangerous
    /// direction: the seal's walk skipped the two strings it may not
    /// edit — a thinking block's signature and the model id — and the
    /// encoder wrote both. Measured, a four-thousand-character signature
    /// gave `manifest says: 12 bytes` over a 4252-byte body. Counting
    /// and editing are now separate questions asked of one walk, so a
    /// string that cannot be cut is still weighed.
    ///
    /// What it does not include is the wire format's own envelope —
    /// braces, field names, the counter this program puts in place of a
    /// tool call's id. Those are not the user's text and there is
    /// nothing in them that came off the user's machine.
    pub bytes: usize,
    /// What was removed: one line per kind, with how many of them.
    pub removed: Vec<(String, usize)>,
    /// Anything that makes this manifest worth less than it looks —
    /// most importantly a layer 3 review that did not run.
    pub notes: Vec<String>,
}

impl Manifest {
    /// The manifest as text, for an interface with a line to spare and
    /// no idea how to lay one of these out.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("About to send to {}.\n", self.destination));
        out.push_str(&format!("Why you are being asked: {}.\n", self.why.sentence()));
        out.push_str(&format!("Why it wants to: {}\n", self.reason));
        out.push_str(&format!("Size after redaction: {} bytes\n", self.bytes));

        if self.sources.is_empty() {
            out.push_str("Files: none — this is the conversation only\n");
        } else {
            out.push_str("Files:\n");
            for source in &self.sources {
                out.push_str(&format!(
                    "  {} — {} bytes read, {} sent",
                    source.path.display(),
                    source.bytes_read,
                    source.bytes_sent
                ));
                if source.removed > 0 {
                    out.push_str(&format!(", {} removed", source.removed));
                }
                out.push('\n');
            }
        }

        if self.removed.is_empty() {
            out.push_str("Removed: nothing matched\n");
        } else {
            out.push_str("Removed before sending:\n");
            for (what, count) in &self.removed {
                out.push_str(&format!("  {count} × {what}\n"));
            }
        }

        for note in &self.notes {
            out.push_str(&format!("Note: {note}\n"));
        }
        out
    }
}

impl fmt::Display for Manifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

/// What this session has already shown the user.
///
/// One per conversation. It exists so that "show the manifest when there
/// is something new" is a decision made by a thing that actually knows
/// what is old, rather than by a boolean somebody sets and forgets.
///
/// Nothing here is persisted. A new session shows a manifest again, and
/// that is correct: the user's memory of what they authorised last
/// Tuesday is not consent for this morning.
#[derive(Clone, Debug, Default)]
pub struct Disclosure {
    shown: bool,
    seen: BTreeSet<PathBuf>,
}

impl Disclosure {
    pub fn new() -> Self {
        Disclosure::default()
    }

    /// Record a file whose contents the user has already read in the
    /// conversation — because the agent showed them, or because they
    /// pasted it themselves. Sending it again is not news.
    pub fn seen_already(&mut self, path: impl Into<PathBuf>) {
        self.seen.insert(path.into());
    }

    pub fn has_shown(&self) -> bool {
        self.shown
    }

    /// Whether these sources need a manifest, and why.
    pub fn required_for(&self, sources: &[Source]) -> Option<Why> {
        if !self.shown {
            return Some(Why::FirstEscalation);
        }
        sources
            .iter()
            .any(|source| !self.seen.contains(&source.path))
            .then_some(Why::UnseenFile)
    }

    /// The user has seen a manifest for these sources and said yes.
    ///
    /// Only called after the answer, never before it: this is the record
    /// of what was disclosed, and recording a disclosure that did not
    /// happen would suppress the next one.
    pub fn accepted(&mut self, sources: &[Source]) {
        self.shown = true;
        for source in sources {
            self.seen.insert(source.path.clone());
        }
    }

    /// Every path the user has already seen the contents of.
    pub fn seen(&self) -> Vec<&Path> {
        self.seen.iter().map(PathBuf::as_path).collect()
    }
}
