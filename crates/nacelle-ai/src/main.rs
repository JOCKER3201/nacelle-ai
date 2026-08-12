//! Standalone mode: the agent as its own program.
//!
//! The same core also runs inside nacelle-desktop as a widget, so
//! everything that is not about being a process lives in
//! `nacelle-ai-core` and nothing here may grow a second implementation
//! of it. What belongs here is what a widget gets from its host
//! instead: the process environment, the terminal, the exit code, and
//! later a window of its own.
//!
//! There is no window yet. This binary exists so the pieces underneath
//! it are built and exercised from the start; until a backend is wired
//! up it reports which credential it found and stops.

use std::process::ExitCode;

use nacelle_ai::credentials::{self, ProcessEnv};

fn main() -> ExitCode {
    match credentials::resolve(&ProcessEnv) {
        Ok(resolved) => {
            // The kind and the origin answer "which token am I using"
            // without printing it. The secret has no path to this
            // output at all: it is inside a Secret, whose Display and
            // Debug both redact.
            println!("nacelle-ai {}", env!("CARGO_PKG_VERSION"));
            println!(
                "credential: {} from {}",
                resolved.credential.kind(),
                resolved.origin
            );
            println!("no backend is wired up yet — that is the next phase.");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("nacelle-ai: {err}");
            ExitCode::FAILURE
        }
    }
}
