//! Why a tool did not do what it was asked.
//!
//! Every variant is something the MODEL can act on, so `Display` is
//! written for that reader rather than for a log: it says what was
//! wrong and, where there is one, what would have been right. A failed
//! tool call is a normal result here — it comes back as a tool result
//! with `is_error` set, the model reads it and tries something else —
//! so nothing in this module panics and nothing is swallowed.
//!
//! Paths appear in these messages because the user is entitled to know
//! which file was refused. Nothing else does: no file contents, and
//! nothing that came from a credential.

use std::error::Error;
use std::fmt;
use std::path::PathBuf;

/// What went wrong in a tool call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolError {
    /// The environment names no place to write: neither
    /// `XDG_CONFIG_HOME` nor `HOME` is set. Refusing beats guessing —
    /// the desktop's own fallback is the process's current directory,
    /// and an agent writing configuration into whatever directory it
    /// happened to be started from is a surprise, not a service.
    NoConfigDir,
    /// No tool of that name. The model was told the names; this is the
    /// answer when it invents one anyway.
    UnknownTool { name: String },
    /// The arguments are not the shape the tool declared.
    BadInput { tool: &'static str, reason: String },
    /// The arguments parsed but the value is not one this may write.
    /// Nothing was touched on disk: validation happens before the file
    /// does.
    Rejected { reason: String },
    /// Nothing installed under that name.
    NotFound { what: String },
    /// The path resolved outside the directory the tool is allowed in —
    /// through `..`, through an absolute path, or through a symlink
    /// pointing out of the tree. A refusal, never a write.
    Outside { input: String, root: PathBuf },
    /// The file is bigger than a tool result should ever carry. Better
    /// to say so than to hand a model a megabyte of text.
    TooLarge { path: PathBuf, size: u64, limit: u64 },
    /// The filesystem said no.
    Io { path: PathBuf, reason: String },
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolError::NoConfigDir => f.write_str(
                "no configuration directory: neither XDG_CONFIG_HOME nor HOME is set, \
                 so there is nowhere this may write",
            ),
            ToolError::UnknownTool { name } => {
                write!(f, "no such tool: {name}")
            }
            ToolError::BadInput { tool, reason } => {
                write!(f, "{tool}: {reason}")
            }
            ToolError::Rejected { reason } => {
                write!(f, "refused, nothing was written: {reason}")
            }
            ToolError::NotFound { what } => {
                write!(f, "not installed: {what}")
            }
            ToolError::Outside { input, root } => write!(
                f,
                "refused: \"{input}\" resolves outside {} — a tool may only touch \
                 files inside that directory",
                root.display()
            ),
            ToolError::TooLarge { path, size, limit } => write!(
                f,
                "{} is {size} bytes, over the {limit}-byte limit for a tool result; \
                 read it with an editor instead",
                path.display()
            ),
            ToolError::Io { path, reason } => {
                write!(f, "{}: {reason}", path.display())
            }
        }
    }
}

impl Error for ToolError {}

impl ToolError {
    /// An I/O failure, with the path it happened to. The `io::Error` is
    /// reduced to its message here on purpose: it is about to be shown
    /// to a model, and its `Display` is the only part that helps.
    pub(crate) fn io(path: impl Into<PathBuf>, err: &std::io::Error) -> Self {
        ToolError::Io {
            path: path.into(),
            reason: err.to_string(),
        }
    }
}
