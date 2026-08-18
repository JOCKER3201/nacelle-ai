//! The nacelle AI daemon: the agent core behind a Unix socket.
//!
//! The window is gone — the owner's decision of 2026-08-16, written out
//! in `.gap-program/decyzja-nacelle-ai-daemon.md`. What used to be a
//! winit window over the toolkit is now four widgets in nacelle-addons,
//! and this program is the process they all talk to.
//!
//! | module | what it is |
//! |---|---|
//! | [`conf`] | the daemon's own settings: `nacelle-ai.ron`, user over system |
//! | [`proto`] | protocol v0: the commands and the events, one JSON object per line |
//! | [`serve`] | one connection: commands in, events out, approvals held open |
//! | [`socket`] | where the socket lives and who may open it |
//! | [`media`] | the `loop` tool: ffmpeg by exec, results to a NEW file |
//! | [`backends`] | which model answers an `ask`, built lazily per connection |
//!
//! Two rules govern everything here, and both are the owner's:
//!
//! **Nothing without a command.** The daemon takes no action of its own
//! initiative — no timers, no watchers, no autonomous work. The core's
//! `Watch` exists and stays UNPLUGGED. The accept loop waits, a
//! connection waits, and the first byte of work happens because a
//! command arrived on the socket. A test holds the daemon open and
//! measures that nothing happens; see `tests/daemon_idle.rs`.
//!
//! **The local model manages the interface, and nothing else.** Ollama
//! is for driving the desktop's own configuration — the agent whose
//! tools are the nacelle tools — and for chat ONLY when the client
//! explicitly picked the `local` backend. It is never handed the user's
//! files to process: the media tools below use no model at all, they
//! are ffmpeg by exec. See [`backends`] for where this is enforced.
//!
//! The confidentiality line (redact, supervise, manifest) is the core's
//! and crosses this crate in one place: approvals and manifests travel
//! to the client as `ev:approval` and come back as `cmd:approve` — per
//! action, no "allow all", and an abandoned question is a refusal.

pub mod backends;
pub mod conf;
pub mod media;
pub mod proto;
pub mod serve;
pub mod socket;
