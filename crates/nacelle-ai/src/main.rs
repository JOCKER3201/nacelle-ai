//! The nacelle AI daemon: parse the command line, open the socket,
//! serve connections. Everything else lives in the library —
//! [`nacelle_ai_daemon`] — where the tests can reach it.
//!
//! There is no window here and there will not be one: the owner's
//! decision of 2026-08-16 (`.gap-program/decyzja-nacelle-ai-daemon.md`)
//! made this program a daemon, and the widgets that draw its answers
//! live in nacelle-addons. The daemon does NOTHING without a command
//! from the socket — no timers, no watchers; the core's `Watch` stays
//! unplugged.

use std::process::ExitCode;
use std::thread;

use nacelle_ai::credentials::ProcessEnv;
use nacelle_ai::Toolbox;
use nacelle_ai_daemon::proto::Wanted;
use nacelle_ai_daemon::{backends, serve, socket};

const USAGE: &str = "\
nacelle-ai — the nacelle agent, as a daemon

    nacelle-ai [--backend auto|claude|local] [--model <id>]

The daemon listens on a Unix socket and does nothing without a command
arriving on it. The socket is $XDG_RUNTIME_DIR/nacelle/ai.sock
(directory 0700, socket 0600), or /tmp/nacelle-$UID/ai.sock when
XDG_RUNTIME_DIR is unset. The protocol is v0 JSON Lines — see the
README.

    --backend   what an ask that says `auto` resolves to. `auto` (the
                default) is the local model with the desktop's own
                configuration tools — interface management, which is
                all the local model is for. `claude` sends those asks
                to Claude instead, behind the manifest. `local` PINS
                the daemon: nothing goes off this machine, and asking
                for claude is refused with the pin.
                An ask that names its own backend always wins.
    --model     which model to ask for. Defaults to the backend's own
                default: the first model the local server reports, or
                claude-opus-4-8.
    --version   print the version and stop.
    --help      print this and stop.

A credential is only needed for Claude. See the README for where it is
looked for; it is never printed, logged, or written into an error.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut want = Wanted::Auto;
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
                let Some(chosen) = Wanted::of(name) else {
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

    // Said here and only here, which is once per run: the core is a
    // library and does not print, and this is the one place the
    // directories are built from the real environment. A machine from
    // before the folder was named after the family says nothing at all.
    let tools = Toolbox::from_env(&ProcessEnv);
    for dir in tools.dirs().legacy_dirs_in_use() {
        eprintln!(
            "nacelle-ai: reading {} \u{2014} the folder's old name. Nothing has been moved \
             and nothing has to be; its place from now on is {}, one folder for the whole \
             nacelle family",
            dir.display(),
            dir.with_file_name(nacelle_ai::tools::paths::APP).display()
        );
    }
    drop(tools);

    let (listener, path) = match socket::listen_from_env(&|name| std::env::var(name).ok()) {
        Ok(bound) => bound,
        Err(msg) => return fail(&msg),
    };
    eprintln!("nacelle-ai: listening on {}", path.display());

    // The accept loop: wait, serve, wait. Each connection gets a thread
    // and a world of its own — sessions are per connection, so two
    // widgets never share a conversation by accident.
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let Ok(reader) = stream.try_clone() else {
            continue;
        };
        let model = model.clone();
        let _ = thread::Builder::new()
            .name("nacelle-ai-conn".to_string())
            .spawn(move || {
                let mut world = backends::Real::new(want, model);
                serve::run(reader, stream, &mut world);
            });
    }
    ExitCode::SUCCESS
}

fn fail(message: &str) -> ExitCode {
    eprintln!("nacelle-ai: {message}");
    ExitCode::FAILURE
}
