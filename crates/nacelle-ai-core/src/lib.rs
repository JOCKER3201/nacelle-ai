//! The agent core: everything that is true of the nacelle AI agent
//! whether it runs in its own window or inside the desktop as a widget.
//!
//! | module | what it is |
//! |---|---|
//! | [`credentials`] | where a token comes from, and which headers each kind carries |
//! | [`message`] | the conversation and the tools the model may call |
//! | [`event`] | what a model reply looks like while it is still arriving |
//! | [`backend`] | the contract every provider implements, and the providers |
//! | [`error`] | how a backend fails |
//! | [`tools`] | what the agent can do to the desktop around it |
//! | [`agent`] | the loop: ask, run what was asked for, ask again, stop |
//! | [`redact`] | what never leaves this machine, in four layers |
//! | [`supervise`] | when the local agent asks the remote one, and the background watch |
//!
//! Two rules shape all of it.
//!
//! **No graphics.** Nothing here draws, opens a window or links the
//! toolkit. The agent is a worker: it is handed a request, it produces
//! [`StreamEvent`]s, and whoever owns the frame decides what that looks
//! like. That is what lets the same core serve a standalone binary and a
//! desktop widget without a second implementation.
//!
//! **No async runtime.** The desktop owns a synchronous event loop
//! (winit), so an agent that demanded its own reactor could not live
//! inside it. Everything here is blocking and belongs on a worker
//! thread; results reach the loop through an ordinary
//! [`std::sync::mpsc`] channel.

pub mod agent;
pub mod backend;
pub mod credentials;
pub mod error;
pub mod event;
pub mod message;
pub mod redact;
pub mod supervise;
pub mod tools;

pub use agent::worker::{Cancel, PendingApproval, TurnId, Worker};
pub use agent::{Agent, AgentError, AgentEvent, AgentSink, ApprovalRequest, Approver, Change,
                Completion, Decision, DenyAll, Effect, EnvironmentFact, History, Limits, NoTools,
                ToolOutput, ToolRegistry};
pub use backend::ollama::{ModelInfo, Ollama};
pub use backend::{Backend, EventSink, Flow};
pub use credentials::{Credential, CredentialError, CredentialKind, Env, Origin, ProcessEnv,
                      Resolved, Secret};
pub use error::BackendError;
pub use event::{StopReason, StreamEvent, Usage};
pub use message::{Content, Message, Request, Role, ToolCall, ToolDeclaration};
pub use redact::deny::{Denial, Denylist, Reason};
pub use redact::manifest::{Disclosure, Manifest, Why};
pub use redact::review::{LocalReviewer, NoReview, NotLocal, Removal, Review, Reviewer};
pub use redact::scan::{Finding, Redacted};
pub use redact::{Gathering, Outgoing, Source};
pub use supervise::escalate::{Attempts, Grounds, Policy, Remote, Trigger};
pub use supervise::handoff::{over_channel, ChannelDiscloser, Cleared, Consent, Discloser, Handoff,
                             NobodyToAsk, PendingDisclosure, Sending};
pub use supervise::seal::{payload_of, NotSent, Seal, Sealed, Stop};
pub use supervise::watch::{Check, Observation, Reporter, Reports, Status, Threshold, Watch,
                           WatchHandle};
pub use tools::error::ToolError;
pub use tools::paths::DesktopDirs;
pub use tools::Toolbox;
