//! Standalone mode: the agent as its own program, in a window of its
//! own.
//!
//! The same core runs inside nacelle-desktop as a widget, so everything
//! that is not about being a process lives in `nacelle-ai-core` and
//! nothing here may grow a second implementation of it. What belongs
//! here is what a widget gets from its host instead: the process
//! environment, the arguments, the exit code — and, since this is the
//! standalone mode, the window.
//!
//! | module | what it is |
//! |---|---|
//! | [`choice`] | which of the two providers answers, and why the other cannot |
//! | [`conversation`] | the column of turns, and the rows it is drawn as |
//! | [`keys`] | who a keystroke belongs to and what it means |
//! | [`window`] | winit, Vulkan through nacelle-renderer, and one frame |
//!
//! The window is built entirely on **libnacelle** — the same toolkit
//! nacelle-desktop is built on, taken from its own repository and not
//! from a copy of the desktop. The theme engine and its master
//! `default.theme` decide every colour, length and duration on screen;
//! the model/view core owns the scrolling; the text input owns the
//! caret, the selection and the undo stack. Nothing here has a colour or
//! a spacing of its own, including as a fallback: what the theme does
//! not say, this program does not draw.

mod choice;
mod conversation;
mod keys;
#[cfg(test)]
mod paper;
mod window;

use std::process::ExitCode;

use nacelle_ai::credentials::ProcessEnv;
use nacelle_ai::{Agent, Toolbox, Worker};

use crate::choice::Want;

const USAGE: &str = "\
nacelle-ai — the nacelle agent, in a window of its own

    nacelle-ai [--backend auto|claude|local] [--model <id>]

    --backend   which provider answers. `auto` (the default) is the
                local model, whether or not a credential resolves —
                Claude is asked only when something needs it, and you
                see exactly what would leave this machine first.
                `claude` is you asking for it in as many words.
                `local` pins the session: nothing goes off the machine
                and the agent says what it cannot do instead.
    --model     which model to ask for. Defaults to the backend's own
                default: the first model the local server reports, or
                claude-opus-4-8.
    --version   print the version and stop.
    --help      print this and stop.

A credential is only needed for Claude. See the README for where it is
looked for; it is never printed, logged, or written into an error.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut want = Want::Auto;
    let mut model: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--version" => {
                println!("nacelle-ai {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "--backend" => {
                let Some(name) = args.get(i + 1) else {
                    return fail("--backend needs a name: auto, claude or local");
                };
                let Some(chosen) = choice::want_of(name) else {
                    return fail(&format!(
                        "there is no backend called \"{name}\" — it is auto, claude or local"
                    ));
                };
                want = chosen;
                i += 1;
            }
            "--model" => {
                let Some(id) = args.get(i + 1) else {
                    return fail("--model needs a model id");
                };
                model = Some(id.clone());
                i += 1;
            }
            other => return fail(&format!("nothing here takes \"{other}\"\n\n{USAGE}")),
        }
        i += 1;
    }

    // Layer 4's line to the screen, made before the choice because the
    // choice is what puts a seal on the far end of it. It stays silent
    // for a session that never reaches off this machine, which is every
    // session that nothing escalates.
    let (discloser, manifests) = nacelle_ai::over_channel();

    // Who answers. This blocks — it may ask a local server what it has —
    // and it is done before the window so the first frame already knows
    // what to say, rather than showing a backend it has not checked.
    let mut choice = choice::choose(want, model.as_deref(), discloser);
    let indicator = choice.indicator();

    // The agent, and the thread it lives on. Everything from here is the
    // core's: the loop, the tools, the approval path, the cancellation.
    let worker = match choice.backend.take() {
        Some(backend) => {
            let tools = Toolbox::from_env(&ProcessEnv);
            let agent = Agent::new(backend, Box::new(tools), choice.model.clone());
            match Worker::spawn(agent) {
                Ok(pair) => Some(pair),
                Err(err) => return fail(&format!("cannot start the agent thread: {err}")),
            }
        }
        None => None,
    };

    match window::run(indicator, choice.notices, worker, manifests) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => fail(&message),
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("nacelle-ai: {message}");
    ExitCode::FAILURE
}
