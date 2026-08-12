//! Which of the two providers answers, and why the other one cannot.
//!
//! The window shows the answer permanently, because it is a fact about
//! the whole conversation and not about one turn: a reply from a model
//! on this machine and a reply from Claude are not the same reply, and a
//! person who cannot see which they got cannot judge either.
//!
//! **The local model is the first responder, and a credential does not
//! change that.** A machine with a token and a running Ollama answers
//! from Ollama. Claude is reached when something makes it necessary —
//! the user asking in as many words, the local model failing the same
//! task twice, work that does not fit its context, a capability it does
//! not have, or its own request with a reason attached — and the list is
//! in `docs/supervisor.md` rather than here. This module implements the
//! first of those triggers, which is the one that can be given before
//! the window opens: `--backend claude` is the user asking.
//!
//! That is the opposite of what an "auto" mode usually means, and it is
//! deliberate. "Whichever is available" reads as convenience and behaves
//! as a policy: on any machine with a token it makes the remote model
//! the default reader of the user's files, permanently, without anybody
//! having decided so. The local half is the one that can be given eyes;
//! the remote half is the one that has to be asked for.
//!
//! The choice is made once, before the window opens, and it is made by
//! ASKING rather than by assuming. A credential either resolves or it
//! does not; a local server either answers `/api/tags` or it does not.
//! Both failures end up on screen as a notice that says what to do,
//! rather than as a window that is simply inert.

use nacelle_ai::backend::anthropic::{self, Anthropic};
use nacelle_ai::credentials::{self, ProcessEnv};
use nacelle_ai::{Backend, ChannelDiscloser, LocalReviewer, Ollama, Policy, Remote, Seal, Trigger};

use crate::conversation::Notice;

/// Which provider the user asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Want {
    /// The supervisor's own order: the local model answers, and Claude
    /// is available for the moment something needs it. A token being
    /// present is not such a moment.
    #[default]
    Auto,
    /// The user asked for Claude in as many words, which is a trigger
    /// in its own right and always honoured — after the manifest.
    Anthropic,
    /// Pinned local. The agent says what it cannot do rather than
    /// reaching for the network, whatever else is true of this machine.
    Ollama,
}

/// Who answers, under what name, and what went wrong on the way here.
pub struct Choice {
    /// `None` when nothing can answer. The window still opens: the
    /// notices are the thing worth showing.
    pub backend: Option<Box<dyn Backend>>,
    /// What goes in [`nacelle_ai::message::Request::model`].
    pub model: String,
    /// The indicator's text — which of the two is answering, in the
    /// words a person uses for them.
    pub label: String,
    /// A second line for the indicator: where the answer comes from.
    pub detail: String,
    /// The indicator's severity role, from the theme's closed set.
    pub severity: &'static str,
    /// Everything worth saying about this choice, in order: who answers,
    /// what may be asked of the other half, and whatever failed on the
    /// way here.
    pub notices: Vec<Notice>,
}

/// What the window's indicator says.
///
/// Everything the window needs from a [`Choice`] once the backend itself
/// has gone off to the worker thread — which is why it is a type of its
/// own rather than a borrow of the choice: after the hand-over the
/// choice's `backend` is `None`, and a window that read it would report
/// that nothing can answer while an answer was arriving.
#[derive(Clone, Debug)]
pub struct Indicator {
    pub label: String,
    pub detail: String,
    pub model: String,
    pub severity: &'static str,
}

impl Choice {
    pub fn indicator(&self) -> Indicator {
        Indicator {
            label: self.label.clone(),
            detail: self.detail.clone(),
            model: self.model.clone(),
            severity: self.severity,
        }
    }
}

/// Which half answers first, and what the other one looks like from
/// here.
///
/// Separated from [`choose`] because it is the decision, and the
/// decision is the thing worth testing: everything around it opens
/// sockets, and a rule that can only be checked by running the program
/// against a live machine is a rule nobody checks.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Plan {
    /// The local model answers. `remote` is what the other half looks
    /// like, which decides what the session may do later and what it
    /// tells the user it cannot.
    Local { pinned: bool, remote: Remote },
    /// Claude answers, because the user asked for it.
    Remote,
    /// The one the user named cannot answer at all.
    Neither,
}

/// The plan, given what the machine turned out to have.
fn plan(want: Want, remote: Remote) -> Plan {
    match want {
        // The rule this whole module exists for: `remote` is carried,
        // not consulted. A resolved credential says what could be asked
        // later; it does not make Claude the first responder, and the
        // reading where it does is the one the owner of this project
        // ruled out.
        Want::Auto => Plan::Local {
            pinned: false,
            remote,
        },
        // A pin does not care what else is on the machine, and it is
        // told back to the user as the pin rather than as a missing
        // token they never asked about — so the remote half is recorded
        // as ready and the pin is what blocks.
        Want::Ollama => Plan::Local {
            pinned: true,
            remote: Remote::Ready,
        },
        // Named by the user, so it is that or nothing: substituting the
        // local model would answer a different question than the one
        // asked.
        Want::Anthropic => match remote {
            Remote::Ready => Plan::Remote,
            _ => Plan::Neither,
        },
    }
}

/// Makes the choice. Blocking: it may contact a local server.
pub fn choose(want: Want, model: Option<&str>, discloser: ChannelDiscloser) -> Choice {
    let mut notices = Vec::new();

    // What the remote half looks like from here, asked once. A pinned
    // session does not ask at all — a token it has been told not to use
    // is not a question worth reading a file for.
    let mut credential = None;
    let remote = match want {
        Want::Ollama => Remote::Ready,
        _ => match credentials::resolve(&ProcessEnv) {
            Ok(resolved) => {
                credential = Some(resolved);
                Remote::Ready
            }
            Err(err) => Remote::NoCredential(err.to_string()),
        },
    };
    // Kept because the plan consumes the remote state and a refusal has
    // to say what was actually wrong. Reading the credential a second
    // time to find out would be a second answer to a question already
    // asked, and they can differ.
    let no_credential = match &remote {
        Remote::NoCredential(detail) => Some(detail.clone()),
        _ => None,
    };

    match plan(want, remote) {
        Plan::Local { pinned, remote } => {
            let policy = if pinned {
                Policy::local_only()
            } else {
                Policy::new(remote)
            };
            notices.push(Notice::Supervision {
                detail: local_first(&policy),
            });
            local(model, notices)
        }

        Plan::Remote => {
            // Unwrapped rather than matched: `Plan::Remote` is only
            // reached through `Remote::Ready`, and that is only reached
            // by a credential resolving above.
            let Some(resolved) = credential else {
                return none(notices);
            };
            // The origin is safe to show and worth showing: "which
            // token am I even using" is the first question when a
            // request is rejected. The secret itself has no path here —
            // it is inside a `Secret`, which redacts.
            let detail = format!("{} from {}", resolved.credential.kind(), resolved.origin);

            let mut seal = Seal::new(
                anthropic::NAME,
                Policy::new(Remote::Ready),
                // The user asked, before the window even opened. That
                // is the trigger, it is recorded as the trigger, and it
                // is what the manifest gives as the reason.
                Trigger::UserAsked,
                discloser,
            );
            match layer_three() {
                Some(reviewer) => seal = seal.with_reviewer(reviewer),
                None => notices.push(Notice::Supervision {
                    detail: NO_LAYER_THREE.to_string(),
                }),
            }
            notices.push(Notice::Supervision {
                detail: ASKED_FOR_CLAUDE.to_string(),
            });

            Choice {
                backend: Some(Box::new(Anthropic::new(resolved.credential, seal))),
                model: model.unwrap_or(anthropic::DEFAULT_MODEL.id).to_string(),
                label: "CLAUDE".into(),
                detail,
                severity: "ok",
                notices,
            }
        }

        Plan::Neither => {
            notices.push(Notice::NoCredential {
                detail: no_credential
                    .unwrap_or_else(|| "no credential resolved on this machine".to_string()),
            });
            none(notices)
        }
    }
}

/// The local half, once it is known to be the one answering.
fn local(model: Option<&str>, mut notices: Vec<Notice>) -> Choice {
    let ollama = Ollama::from_env(&ProcessEnv);
    let host = ollama.host().to_string();

    match ollama.models() {
        Ok(models) if !models.is_empty() => {
            let id = match model {
                Some(asked) if models.iter().any(|m| m.name == asked) => asked.to_string(),
                Some(asked) => {
                    notices.push(Notice::NoOllama {
                        detail: format!(
                            "{host} has no model called \"{asked}\" — using {} instead",
                            models[0].name
                        ),
                    });
                    models[0].name.clone()
                }
                None => models[0].name.clone(),
            };
            Choice {
                backend: Some(Box::new(ollama)),
                model: id,
                label: "LOCAL MODEL".into(),
                detail: host,
                severity: "ok",
                notices,
            }
        }
        Ok(_) => {
            notices.push(Notice::NoOllama {
                detail: format!(
                    "{host} is running but has no models pulled — `ollama pull <name>` puts \
                     one there"
                ),
            });
            none(notices)
        }
        Err(err) => {
            notices.push(Notice::NoOllama {
                detail: err.to_string(),
            });
            none(notices)
        }
    }
}

/// Layer 3's model, when this machine has one.
///
/// Asked for even when the user named Claude: the review runs on the
/// payload that is about to leave, and it is the only layer that can see
/// meaning rather than shape. `None` when there is no local model —
/// which is a different thing from a review that found nothing, and the
/// manifest says which of the two happened.
fn layer_three() -> Option<LocalReviewer> {
    let ollama = Ollama::from_env(&ProcessEnv);
    let name = ollama.models().ok()?.first()?.name.clone();
    // Refuses anything that is not on the loopback interface: asking a
    // model on another machine whether a payload may be sent sends it.
    LocalReviewer::new(Box::new(ollama), name).ok()
}

/// Nothing can answer. The window opens anyway, saying so.
fn none(mut notices: Vec<Notice>) -> Choice {
    notices.push(Notice::NoBackend);
    Choice {
        backend: None,
        model: String::new(),
        label: "NO BACKEND".into(),
        detail: "nothing can answer".into(),
        severity: "offline",
        notices,
    }
}

/// What the user is told when the local model is answering.
///
/// [`Policy::status`] is the half that is a fact about this machine; the
/// rest is what to do about it, which a status line cannot say and a
/// person reading it for the first time needs.
fn local_first(policy: &Policy) -> String {
    if policy.is_pinned() {
        return "This session is pinned to the local model. Nothing goes off this machine, \
                and the agent will say what it cannot do rather than reaching for the \
                network."
            .to_string();
    }
    format!(
        "{}.\nThe local model answers; Claude is asked only when something needs it — you \
         asking for it, the same task failing twice, work that does not fit, or a capability \
         the local model has not got. Nothing leaves this machine until you have seen what \
         would leave and agreed to it.",
        policy.status()
    )
}

/// Said when the user named Claude on the command line.
const ASKED_FOR_CLAUDE: &str = "\
You asked for Claude, so this session starts on it rather than on the local model. Before \
the first thing goes off this machine you will be shown exactly what would go — which \
files, how many bytes, and what was removed — and it goes only if you say so.";

/// Said when there is no local model to run layer 3.
const NO_LAYER_THREE: &str = "\
There is no local model here to read the payload before it goes, so only the pattern rules \
run on it: keys, tokens, passwords and the like are still cut out, but nothing checks for \
private MEANING — a diagnosis, a private message, an unreleased plan. The manifest says so \
too. Start `ollama serve` with a model pulled and that layer runs.";

/// The provider named on the command line.
pub fn want_of(name: &str) -> Option<Want> {
    match name {
        "auto" => Some(Want::Auto),
        "anthropic" | "claude" => Some(Want::Anthropic),
        "ollama" | "local" => Some(Want::Ollama),
        _ => None,
    }
}

// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nacelle::ui;

    #[test]
    fn the_provider_can_be_named_by_either_of_its_names() {
        assert_eq!(want_of("anthropic"), Some(Want::Anthropic));
        assert_eq!(want_of("claude"), Some(Want::Anthropic));
        assert_eq!(want_of("ollama"), Some(Want::Ollama));
        assert_eq!(want_of("local"), Some(Want::Ollama));
        assert_eq!(want_of("auto"), Some(Want::Auto));
        assert_eq!(want_of("gpt"), None);
    }

    #[test]
    fn the_indicator_reads_in_severities_the_theme_declares() {
        for severity in ["ok", "offline"] {
            assert!(ui::sev_of(severity).is_some());
        }
    }

    #[test]
    fn with_nothing_to_ask_the_window_is_told_so_last() {
        let choice = none(vec![Notice::NoCredential {
            detail: "none".into(),
        }]);
        assert!(choice.backend.is_none());
        assert_eq!(choice.notices.len(), 2);
        assert_eq!(choice.notices[1], Notice::NoBackend);
    }

    /// The whole point of the module, and the thing that was the wrong
    /// way round: a machine with a working token still answers from the
    /// local model when nobody asked for anything else.
    #[test]
    fn the_default_is_the_local_model_even_with_a_credential_in_hand() {
        assert_eq!(
            plan(Want::Auto, Remote::Ready),
            Plan::Local {
                pinned: false,
                remote: Remote::Ready
            }
        );
    }

    #[test]
    fn a_machine_with_no_credential_makes_no_difference_to_who_answers() {
        let missing = Remote::NoCredential("no token here".into());
        assert_eq!(
            plan(Want::Auto, missing.clone()),
            Plan::Local {
                pinned: false,
                remote: missing
            }
        );
    }

    /// No token and no network are not two failures with two
    /// behaviours: they are the pin, arrived at by accident.
    #[test]
    fn no_token_and_no_network_leave_the_session_where_a_pin_leaves_it() {
        for remote in [
            Remote::NoCredential("no token".into()),
            Remote::Unreachable("no route to host".into()),
        ] {
            let Plan::Local { pinned, remote } = plan(Want::Auto, remote) else {
                panic!("the local model answers whatever the remote half is doing");
            };
            assert!(!pinned);
            let policy = Policy::new(remote);
            assert!(policy.blocked().is_some());
            assert!(policy.status().starts_with("local model only"));
        }
    }

    #[test]
    fn asking_for_the_local_model_pins_the_session_whatever_else_is_here() {
        let Plan::Local { pinned, .. } = plan(Want::Ollama, Remote::Ready) else {
            panic!("naming the local model must not reach for anything else");
        };
        assert!(pinned);
        assert!(Policy::local_only().blocked().is_some());
    }

    #[test]
    fn naming_claude_is_the_user_asking_and_it_is_honoured() {
        assert_eq!(plan(Want::Anthropic, Remote::Ready), Plan::Remote);
    }

    /// Named by the user and not available: say so. Quietly answering
    /// from the local model would answer a question nobody asked.
    #[test]
    fn naming_claude_without_a_credential_answers_with_nothing() {
        assert_eq!(
            plan(Want::Anthropic, Remote::NoCredential("none".into())),
            Plan::Neither
        );
    }

    #[test]
    fn a_pinned_session_is_told_it_is_pinned_rather_than_told_about_a_token() {
        let said = local_first(&Policy::local_only());
        assert!(said.contains("pinned"));
        assert!(!said.contains("credential"));
    }
}
