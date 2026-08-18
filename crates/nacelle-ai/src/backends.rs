//! Which model answers an `ask`, built lazily per connection — and the
//! POLICY, which is the owner's and is enforced here:
//!
//! **The local model (Ollama) is for managing the interface, and for
//! chat the client chose in as many words. Nothing else.**
//!
//! Concretely, the local model runs in exactly two places:
//!
//! * an `ask` whose backend is `auto` — the daemon's own agent, whose
//!   tools are the nacelle configuration tools ([`Toolbox`]): themes,
//!   layauts, `nacelle-desktop.ron`. That is interface management.
//! * an `ask` whose backend is `local` — chat the client pinned to this
//!   machine, explicitly, per command. Choosing it is what permits it.
//!
//! It is **never** handed the user's files to process. The media tools
//! (`loop`, and `photo`/`sort` when they exist) involve no model at all
//! — they are deterministic, ffmpeg by exec — and there is no path from
//! a `tool` command to a model of any kind. See `serve`, where the two
//! kinds of command part ways.
//!
//! The remote half keeps the confidentiality line it always had: an
//! `ask` on `claude` goes through the core's seal — redact, the local
//! review when a local model is present, and the manifest, which
//! travels to the client as `ev:approval` and waits for `cmd:approve`.
//!
//! [`World`] is the seam the tests use: `serve` talks to the trait, the
//! daemon hands it [`Real`], and a test hands it scripted sessions with
//! no network anywhere.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use nacelle_ai::backend::anthropic::{self, Anthropic};
use nacelle_ai::backend::ollama::HOST_VAR;
use nacelle_ai::credentials::{self, Env, ProcessEnv};
use nacelle_ai::{Agent, AgentEvent, Limits, LocalReviewer, Ollama, PendingDisclosure, Policy,
                 Remote, Seal, Toolbox, Trigger, Worker};

use crate::media::Ffmpeg;
use crate::proto::Wanted;

/// One agent behind one connection: the worker it runs on, the events
/// it produces, and — for a session whose bytes can leave the machine —
/// layer 4's manifests, which `serve` turns into `ev:approval`.
pub struct Session {
    pub worker: Worker,
    pub events: Receiver<AgentEvent>,
    pub manifests: Option<Receiver<PendingDisclosure>>,
}

/// Everything `serve` needs from the machine, as a trait so the tests
/// can hand in a machine that does not exist.
pub trait World {
    /// The names `ev:hello` reports — which backends could answer here.
    fn backends(&mut self) -> Vec<String>;

    /// A session for what the `ask` named. Called once per backend per
    /// connection; `serve` keeps the result, so the conversation
    /// survives from one `ask` to the next.
    fn session(&mut self, asked: Wanted) -> Result<Session, String>;

    /// The ffmpeg the `loop` tool will exec.
    fn ffmpeg(&mut self) -> Result<Ffmpeg, String>;
}

/// The real machine: credentials from the process environment, Ollama
/// over localhost, Anthropic behind the seal.
pub struct Real {
    /// The daemon's own default: what an `ask` that says `auto`
    /// resolves to. `local` here is a pin — see [`Real::session`].
    /// `--backend`, or `backend:` in `nacelle-ai.ron`.
    want: Wanted,
    /// `--model`, or `model:` in the file.
    model: Option<String>,
    /// `ollama_host:` from the file. `OLLAMA_HOST` outranks it — see
    /// [`Real::ollama`].
    host: Option<String>,
    /// `ffmpeg:` from the file. `NACELLE_AI_FFMPEG` outranks it.
    ffmpeg: Option<PathBuf>,
    /// Where the agent loop gives up. The built-in ceiling until the
    /// file moves it; it was unreachable from anywhere before there was
    /// a file.
    limits: Limits,
}

impl Real {
    /// The machine with everything at its built-in setting.
    pub fn new(want: Wanted, model: Option<String>) -> Real {
        Real {
            want,
            model,
            host: None,
            ffmpeg: None,
            limits: Limits::default(),
        }
    }

    /// The Ollama server `nacelle-ai.ron` named, if it named one.
    pub fn with_host(mut self, host: Option<String>) -> Real {
        self.host = host;
        self
    }

    /// The ffmpeg `nacelle-ai.ron` named, if it named one.
    pub fn with_ffmpeg(mut self, ffmpeg: Option<PathBuf>) -> Real {
        self.ffmpeg = ffmpeg;
        self
    }

    pub fn with_limits(mut self, limits: Limits) -> Real {
        self.limits = limits;
        self
    }

    /// The local server this daemon talks to.
    fn ollama(&self) -> Ollama {
        ollama_at(&ProcessEnv, self.host.as_deref())
    }
}

/// Which Ollama server is asked, given the environment and what
/// `nacelle-ai.ron` said.
///
/// `OLLAMA_HOST` first, because that is the variable Ollama's own tools
/// read and a person exports it for a session; then the file; then the
/// built-in `http://localhost:11434`. The middle of those three is the
/// new one, and it is deliberately not on top: see the order written
/// out in [`conf`](crate::conf).
///
/// A free function over an injected environment, rather than a method
/// reading the process's own, because THREE places have to give the
/// same answer: the session this daemon builds, the line it prints when
/// it starts, and `--print-config`. Until 2026-08-18 the rule lived
/// inside [`Real::ollama`] where nothing else could reach it, so the
/// other two read the file and reported a host the daemon was not
/// asking — which is the one thing a report about settings must not do.
pub fn ollama_at(env: &dyn Env, configured: Option<&str>) -> Ollama {
    let exported = env
        .var(HOST_VAR)
        .filter(|v| !v.trim().is_empty())
        .is_some();
    match (configured, exported) {
        (Some(host), false) => Ollama::at(host),
        _ => Ollama::from_env(env),
    }
}

/// What the daemon says when it is pinned and Claude is asked for.
const PINNED: &str = "this daemon is pinned to the local model (--backend local): nothing \
                      goes off this machine, and Claude cannot be asked";

impl World for Real {
    fn backends(&mut self) -> Vec<String> {
        let mut names = Vec::new();
        // Local first, because local answers first. Reported only when
        // it could actually answer — a server with a model pulled.
        if self
            .ollama()
            .models()
            .map(|m| !m.is_empty())
            .unwrap_or(false)
        {
            names.push("local".to_string());
        }
        // A pinned daemon does not offer Claude, whatever is on the
        // machine: offering what a command would then refuse is a lie.
        if self.want != Wanted::Local && credentials::resolve(&ProcessEnv).is_ok() {
            names.push("claude".to_string());
        }
        names
    }

    fn session(&mut self, asked: Wanted) -> Result<Session, String> {
        // `auto` is the daemon deciding, and the daemon's decision is
        // its command line. The default command line leaves `auto` as
        // `auto`: the interface-management agent on the local model.
        let resolved = match asked {
            Wanted::Auto => self.want,
            named => named,
        };
        match resolved {
            Wanted::Claude if self.want == Wanted::Local => Err(PINNED.to_string()),
            Wanted::Claude => claude_session(self.model.as_deref(), self.limits, self.ollama()),
            // `auto` and `local` both run on the local model and both
            // are allowed to — the first is interface management, the
            // second is chat the client chose. They are separate
            // sessions (serve caches by name), so the daemon's own
            // agent and the user's pinned chat do not share a history.
            Wanted::Auto | Wanted::Local => {
                local_session(self.model.as_deref(), self.limits, self.ollama())
            }
        }
    }

    fn ffmpeg(&mut self) -> Result<Ffmpeg, String> {
        Ffmpeg::pick(&|name| std::env::var(name).ok(), self.ffmpeg.as_deref())
    }
}

/// The local model, with the nacelle tools.
fn local_session(model: Option<&str>, limits: Limits, ollama: Ollama) -> Result<Session, String> {
    let host = ollama.host().to_string();
    let models = ollama
        .models()
        .map_err(|e| format!("the local model is not answering: {e}"))?;
    if models.is_empty() {
        return Err(format!(
            "{host} is running but has no models pulled — `ollama pull <name>` puts one there"
        ));
    }
    let id = match model {
        // Named and absent is an error, not a substitution: the daemon
        // has no window to put a notice in, and quietly answering from
        // a different model would answer a different question.
        Some(asked) => match models.iter().any(|m| m.name == asked) {
            true => asked.to_string(),
            false => {
                return Err(format!(
                    "{host} has no model called \"{asked}\" — `ollama pull {asked}` puts it there"
                ))
            }
        },
        None => models[0].name.clone(),
    };
    spawn(
        Agent::new(
            Box::new(ollama),
            Box::new(Toolbox::from_env(&ProcessEnv)),
            id,
        )
        .with_limits(limits),
    )
    .map(|(worker, events)| Session {
        worker,
        events,
        manifests: None,
    })
}

/// Claude, behind the whole confidentiality line.
fn claude_session(model: Option<&str>, limits: Limits, ollama: Ollama) -> Result<Session, String> {
    let resolved = credentials::resolve(&ProcessEnv).map_err(|e| e.to_string())?;
    let (discloser, manifests) = nacelle_ai::over_channel();
    let mut seal = Seal::new(
        anthropic::NAME,
        Policy::new(Remote::Ready),
        // The client asked for Claude by name in the command. That is
        // the trigger, and it is what the manifest gives as the reason.
        Trigger::UserAsked,
        discloser,
    );
    // Layer 3, when this machine has a local model to run it. Its
    // absence is a weaker line, not a broken one — the pattern rules
    // still run, and the manifest says which of the two happened.
    if let Some(reviewer) = layer_three(ollama) {
        seal = seal.with_reviewer(reviewer);
    }
    let id = model.unwrap_or(anthropic::DEFAULT_MODEL.id).to_string();
    spawn(
        Agent::new(
            Box::new(Anthropic::new(resolved.credential, seal)),
            Box::new(Toolbox::from_env(&ProcessEnv)),
            id,
        )
        .with_limits(limits),
    )
    .map(|(worker, events)| Session {
        worker,
        events,
        manifests: Some(manifests),
    })
}

/// Layer 3's model, when this machine has one.
///
/// [`LocalReviewer::new`] refuses anything that is not on the loopback
/// interface: asking a model on another machine whether a payload may
/// be sent sends it. That refusal is why a configured `ollama_host`
/// pointing off this machine costs layer 3 rather than moving it —
/// unchanged by the configuration file, which only decides WHICH host
/// is asked, never whether the rule applies.
fn layer_three(ollama: Ollama) -> Option<LocalReviewer> {
    let name = ollama.models().ok()?.first()?.name.clone();
    LocalReviewer::new(Box::new(ollama), name).ok()
}

fn spawn(agent: Agent) -> Result<(Worker, Receiver<AgentEvent>), String> {
    Worker::spawn(agent).map_err(|e| format!("cannot start the agent thread: {e}"))
}

// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use nacelle_ai::backend::ollama::DEFAULT_HOST;

    /// An environment that is a table rather than the process's own,
    /// which is shared, racy under a threaded runner, and would let a
    /// developer's exported OLLAMA_HOST decide whether a test passes.
    struct Table(Option<&'static str>);

    impl Env for Table {
        fn var(&self, key: &str) -> Option<String> {
            match key == HOST_VAR {
                true => self.0.map(str::to_string),
                false => None,
            }
        }
    }

    /// The variable a person exported for this run beats the line
    /// written down months ago. This is the rung `--print-config` and
    /// the startup line got wrong while the rule was private.
    #[test]
    fn an_exported_host_beats_the_file() {
        let asked = ollama_at(&Table(Some("http://box:11434")), Some("127.0.0.1:59999"));
        assert_eq!(asked.host(), "http://box:11434");
    }

    #[test]
    fn the_file_answers_when_nothing_is_exported() {
        let asked = ollama_at(&Table(None), Some("127.0.0.1:59999"));
        assert_eq!(asked.host(), "http://127.0.0.1:59999");
    }

    /// A variable exported as blank is a variable nobody set — the same
    /// reading the socket's own placement gives it.
    #[test]
    fn a_blank_exported_host_counts_as_unset() {
        let asked = ollama_at(&Table(Some("   ")), Some("127.0.0.1:59999"));
        assert_eq!(asked.host(), "http://127.0.0.1:59999");
    }

    #[test]
    fn with_neither_the_built_in_host_answers() {
        assert_eq!(ollama_at(&Table(None), None).host(), DEFAULT_HOST);
    }

    /// The policy's edge that can be tested without a machine: a pinned
    /// daemon refuses Claude with the pin, not with a missing token.
    #[test]
    fn a_pinned_daemon_refuses_claude_with_the_pin() {
        let mut real = Real::new(Wanted::Local, None);
        let err = match real.session(Wanted::Claude) {
            Err(err) => err,
            Ok(_) => panic!("a pinned daemon built a Claude session"),
        };
        assert!(err.contains("pinned"), "said: {err}");
        assert!(!err.contains("credential"), "said: {err}");
    }
}
