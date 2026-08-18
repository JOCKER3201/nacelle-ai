//! The nacelle AI daemon: read the configuration, parse the command
//! line over it, open the socket, serve connections. Everything else
//! lives in the library — [`nacelle_ai_daemon`] — where the tests can
//! reach it.
//!
//! There is no window here and there will not be one: the owner's
//! decision of 2026-08-16 (`.gap-program/decyzja-nacelle-ai-daemon.md`)
//! made this program a daemon, and the widgets that draw its answers
//! live in nacelle-addons. The daemon does NOTHING without a command
//! from the socket — no timers, no watchers; the core's `Watch` stays
//! unplugged.
//!
//! **The file is read before the arguments, and the arguments win.**
//! Both halves matter. The desktop starts this program with no
//! arguments at all, so a setting that exists only as a flag is a
//! setting nobody on a real machine can reach — which is what
//! `--backend` and `--model` were until 2026-08-18. And a flag somebody
//! typed a second ago must beat a file written down months ago, so the
//! file is the floor and the command line is laid over it. The one
//! argument that has to be read first is `--config`, because it names
//! the file.

use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;

use nacelle_ai::credentials::ProcessEnv;
use nacelle_ai::Toolbox;
use nacelle_ai_daemon::conf::{self, Chosen, InForce, Loaded, Settled, Source};
use nacelle_ai_daemon::proto::Wanted;
use nacelle_ai_daemon::{backends, serve, socket};

const USAGE: &str = "\
nacelle-ai — the nacelle agent, as a daemon

    nacelle-ai [--backend auto|claude|local] [--model <id>]
               [--config <file>] [--print-config]

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
    --config    read this file instead of the usual two. A file named
                here that is missing or broken stops the daemon; one
                found on the usual search path is reported and skipped.
    --print-config
                print the configuration in force — every setting with
                the rung that decided it, a flag or a variable or a
                file — and where it looked, then stop.
    --version   print the version and stop.
    --help      print this and stop.

The settings this program has that are not flags — the Ollama host, the
socket's place, which ffmpeg, and where the agent loop gives up — live
in nacelle-ai.ron:

    $XDG_CONFIG_HOME/nacelle/nacelle-ai.ron      the user's own
    $XDG_CONFIG_DIRS/nacelle/nacelle-ai.ron      system defaults

The order everywhere is: the command line, then the environment, then
the user's file, then a system file, then this program's own default.

A credential is only needed for Claude. See the README for where it is
looked for; it is never printed, logged, or written into an error.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // --help and --version before anything opens a file: a machine
    // whose configuration is broken must still be able to ask what
    // this program is and which one it has.
    for arg in &args {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--version" => {
                println!("nacelle-ai {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            _ => {}
        }
    }

    // The file, before the arguments are parsed. `--config` is the one
    // argument that must be read first, because it names the file.
    let named = match named_config(&args) {
        Ok(named) => named,
        Err(msg) => return fail(&msg),
    };
    let (loaded, places) = match &named {
        Some(path) => match conf::load_named(path) {
            Ok(loaded) => (loaded, vec![path.clone()]),
            // A file somebody named a second ago and that is not there
            // is a mistake to say out loud, not a rung to skip.
            Err(msg) => return fail(&msg),
        },
        None => (conf::load(&ProcessEnv), conf::places(&ProcessEnv)),
    };
    let settled = loaded.conf.settle();
    for note in loaded.notes.iter().chain(settled.notes.iter()) {
        eprintln!("nacelle-ai: {note}");
    }
    // Now the command line, over what the file said. Parsed before
    // `--print-config` prints, so a line with a typo in it is answered
    // with the typo rather than with a report of settings that line was
    // never going to run under.
    let chosen = match over(&args, &settled) {
        Ok(chosen) => chosen,
        Err(msg) => return fail(&msg),
    };
    // And then the machine, which is the rung above both: an exported
    // OLLAMA_HOST or NACELLE_AI_FFMPEG beats the file. Asked ONCE here,
    // so the report and the startup line below say what the accept loop
    // will actually do rather than what the file alone said.
    let force = conf::in_force(&ProcessEnv, &settled, &chosen);
    if args.iter().any(|a| a == "--print-config") {
        print!("{}", conf::report(&loaded, &settled, &force, &places));
        return ExitCode::SUCCESS;
    }
    say_where_from(&loaded, &force);
    // What every connection below is built on. The same two values the
    // report just printed, because they are the same two values.
    let (want, model) = (chosen.backend, chosen.model);

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

    let bound = match &settled.socket {
        Some(path) => socket::listen_named(path),
        None => socket::listen_from_env(&|name| std::env::var(name).ok()),
    };
    let (listener, path) = match bound {
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
        let host = settled.ollama_host.clone();
        let ffmpeg = settled.ffmpeg.clone();
        let limits = settled.limits;
        let _ = thread::Builder::new()
            .name("nacelle-ai-conn".to_string())
            .spawn(move || {
                let mut world = backends::Real::new(want, model)
                    .with_host(host)
                    .with_ffmpeg(ffmpeg)
                    .with_limits(limits);
                serve::run(reader, stream, &mut world);
            });
    }
    ExitCode::SUCCESS
}

/// `--config <file>`, before anything else is parsed.
fn named_config(args: &[String]) -> Result<Option<PathBuf>, String> {
    let mut found: Option<PathBuf> = None;
    for (i, arg) in args.iter().enumerate() {
        if arg != "--config" {
            continue;
        }
        let Some(path) = args.get(i + 1) else {
            return Err("--config needs a file".to_string());
        };
        found = Some(PathBuf::from(path));
    }
    Ok(found)
}

/// The command line, laid over what the file settled.
///
/// Reports WHICH of the two answered as well as what: a flag and a file
/// that both say `claude` are the same daemon and two different things
/// to go and edit, and `--print-config` is run by somebody who needs to
/// know which.
fn over(args: &[String], settled: &Settled) -> Result<Chosen, String> {
    let mut chosen = Chosen {
        backend: settled.backend.unwrap_or_default(),
        backend_from_line: false,
        model: settled.model.clone(),
        model_from_line: false,
    };

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            // Answered before this function ran; skipped rather than
            // rejected so the loop below can stay strict.
            "--help" | "-h" | "--version" | "--print-config" => {}
            "--config" => i += 1,
            "--backend" => {
                let Some(name) = args.get(i + 1) else {
                    return Err("--backend needs a name: auto, claude or local".to_string());
                };
                let Some(named) = Wanted::of(name) else {
                    return Err(format!(
                        "there is no backend called \"{name}\" — it is auto, claude or local"
                    ));
                };
                chosen.backend = named;
                chosen.backend_from_line = true;
                i += 1;
            }
            "--model" => {
                let Some(id) = args.get(i + 1) else {
                    return Err("--model needs a model id".to_string());
                };
                chosen.model = Some(id.clone());
                chosen.model_from_line = true;
                i += 1;
            }
            other => return Err(format!("nothing here takes \"{other}\"\n\n{USAGE}")),
        }
        i += 1;
    }
    Ok(chosen)
}

/// The trace a daemon leaves of which configuration it is running.
///
/// There is no other: no window, and `--print-config` is a different
/// process with a possibly different environment.
///
/// It therefore has to be TRUE, and until 2026-08-18 the host line was
/// not — it printed what the file said even when an exported
/// OLLAMA_HOST was what the daemon then asked, which is the one case
/// the line exists for. It now reads the resolved [`InForce`], the same
/// value the accept loop runs on, and names the rung that decided.
fn say_where_from(loaded: &Loaded, force: &InForce) {
    for line in where_from(loaded, force) {
        eprintln!("nacelle-ai: {line}");
    }
}

/// The lines themselves, so a test can read them.
fn where_from(loaded: &Loaded, force: &InForce) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(first) = loaded.read.first() {
        let more = match loaded.read.len() - 1 {
            0 => String::new(),
            1 => " and one below it".to_string(),
            n => format!(" and {n} below it"),
        };
        out.push(format!("settings from {}{more}", first.display()));
    }
    // Said when somebody CHOSE the host, whether in the file or in the
    // environment. The built-in localhost is not news and is not
    // printed; a host that came from anywhere else is exactly what a
    // person reading stderr is trying to find out.
    if let Some(from) = force.ollama_host.from.filter(|f| *f != Source::BuiltIn) {
        out.push(format!(
            "the local model is asked at {} ({})",
            force.ollama_host.value,
            from.words()
        ));
    }
    out
}

fn fail(message: &str) -> ExitCode {
    eprintln!("nacelle-ai: {message}");
    ExitCode::FAILURE
}

// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// The file is the floor: with no flags, what it said is what runs.
    #[test]
    fn the_file_answers_when_the_command_line_says_nothing() {
        let settled = conf::parse("(backend: Named(\"local\"), model: Named(\"llama3\"))")
            .expect("this parses")
            .settle();
        let chosen = over(&args(&[]), &settled).expect("nothing to reject");
        assert_eq!(chosen.backend, Wanted::Local);
        assert_eq!(chosen.model.as_deref(), Some("llama3"));
        assert!(!chosen.backend_from_line && !chosen.model_from_line);
    }

    /// And the command line is the ceiling.
    #[test]
    fn a_flag_beats_the_file() {
        let settled = conf::parse("(backend: Named(\"local\"), model: Named(\"llama3\"))")
            .expect("this parses")
            .settle();
        let chosen = over(&args(&["--backend", "claude", "--model", "opus"]), &settled)
            .expect("nothing to reject");
        assert_eq!(chosen.backend, Wanted::Claude);
        assert_eq!(chosen.model.as_deref(), Some("opus"));
        assert!(chosen.backend_from_line && chosen.model_from_line);
    }

    #[test]
    fn with_neither_file_nor_flag_the_daemon_is_auto() {
        let chosen = over(&args(&[]), &Settled::default()).expect("nothing to reject");
        assert_eq!(chosen.backend, Wanted::Auto);
        assert_eq!(chosen.model, None);
    }

    #[test]
    fn config_is_read_off_the_command_line_before_anything_else() {
        assert_eq!(
            named_config(&args(&["--config", "/tmp/x.ron", "--backend", "local"])).unwrap(),
            Some(PathBuf::from("/tmp/x.ron"))
        );
        assert_eq!(named_config(&args(&["--backend", "local"])).unwrap(), None);
        assert!(named_config(&args(&["--config"])).is_err());
    }

    /// `--config` and its value are not rejected by the strict pass —
    /// they were consumed before it ran, and an argument answered twice
    /// is an argument that stops the daemon.
    #[test]
    fn the_arguments_read_early_are_not_rejected_late() {
        for line in [
            args(&["--config", "/tmp/x.ron"]),
            args(&["--print-config"]),
            args(&["--config", "/tmp/x.ron", "--print-config", "--model", "m"]),
        ] {
            over(&line, &Settled::default()).expect("this line is accepted");
        }
    }

    #[test]
    fn an_unknown_flag_still_stops_the_daemon() {
        assert!(over(&args(&["--eyes"]), &Settled::default()).is_err());
        assert!(over(&args(&["--backend", "gpt"]), &Settled::default()).is_err());
        assert!(over(&args(&["--model"]), &Settled::default()).is_err());
    }

    // -----------------------------------------------------------------
    // The one trace the daemon leaves on stderr.

    struct Table(&'static [(&'static str, &'static str)]);

    impl nacelle_ai::credentials::Env for Table {
        fn var(&self, key: &str) -> Option<String> {
            self.0
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_string())
        }
    }

    fn forced(env: &Table, text: &str) -> InForce {
        let settled = conf::parse(text).expect("this parses").settle();
        let chosen = over(&args(&[]), &settled).expect("nothing to reject");
        conf::in_force(env, &settled, &chosen)
    }

    /// The line has to name the host the daemon will ACTUALLY ask.
    /// It named the file's even when an exported OLLAMA_HOST was what
    /// the connections then went to — the one case the line exists for.
    #[test]
    fn the_startup_line_names_the_host_the_daemon_will_ask() {
        let exported = Table(&[("OLLAMA_HOST", "http://localhost:11434")]);
        let force = forced(&exported, "(ollama_host: Named(\"127.0.0.1:59999\"))");
        let said = where_from(&Loaded::default(), &force).join("\n");
        assert!(said.contains("http://localhost:11434"), "said: {said}");
        assert!(said.contains("OLLAMA_HOST"), "said: {said}");
        assert!(!said.contains("59999"), "said: {said}");
    }

    #[test]
    fn the_file_is_named_when_it_is_the_file_that_answered() {
        let force = forced(&Table(&[]), "(ollama_host: Named(\"127.0.0.1:59999\"))");
        let said = where_from(&Loaded::default(), &force).join("\n");
        assert!(said.contains("http://127.0.0.1:59999"), "said: {said}");
        assert!(said.contains("the file"), "said: {said}");
    }

    /// A host nobody chose is not news. The built-in localhost says
    /// nothing about the configuration this daemon is running.
    #[test]
    fn nothing_is_said_about_a_host_nobody_chose() {
        let force = forced(&Table(&[]), "()");
        assert!(
            where_from(&Loaded::default(), &force).is_empty(),
            "said: {:?}",
            where_from(&Loaded::default(), &force)
        );
    }

    #[test]
    fn the_files_that_were_read_are_counted() {
        let read = |n: usize| Loaded {
            read: (0..n).map(|i| PathBuf::from(format!("/f{i}.ron"))).collect(),
            ..Loaded::default()
        };
        let force = forced(&Table(&[]), "()");
        assert_eq!(where_from(&read(1), &force), ["settings from /f0.ron"]);
        assert_eq!(
            where_from(&read(2), &force),
            ["settings from /f0.ron and one below it"]
        );
        assert_eq!(
            where_from(&read(3), &force),
            ["settings from /f0.ron and 2 below it"]
        );
    }
}
