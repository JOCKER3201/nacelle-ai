//! The daemon's own configuration: `nacelle-ai.ron`.
//!
//! Until 2026-08-18 this program had none. Its two flags —
//! `--backend` and `--model` — were the whole of its settings, and the
//! desktop starts it with no arguments at all
//! (`nacelle-desktop/src/main.rs`, `spawn_ai_daemon`), so on a real
//! machine neither flag could ever be reached. Everything else was a
//! constant somewhere: sixteen turns, two hundred thousand bytes of
//! history, the socket's place, whichever ffmpeg happened to be first
//! on `PATH`.
//!
//! ```text
//! $XDG_CONFIG_HOME/nacelle/nacelle-ai.ron      the user's own
//! $XDG_CONFIG_DIRS/nacelle/nacelle-ai.ron      system defaults (/etc/xdg)
//! ```
//!
//! The FOLDER is the family and the FILE is the program — the same
//! arrangement `nacelle-desktop.ron` already sits in, one directory,
//! one file per program. The folder's old name (`nacelle-desktop/`) is
//! read for the desktop's file because machines have one; it is NOT
//! read for this file, because this file has never existed anywhere and
//! reading a place nothing was ever written to only says something
//! false about where settings come from.
//!
//! **One order of precedence, for everything:**
//!
//! ```text
//! the command line  >  the process environment  >  the user's file
//!                   >  a system file            >  the built-in default
//! ```
//!
//! The environment sits above the file on purpose. `OLLAMA_HOST` and
//! `NACELLE_AI_FFMPEG` are what a person exports for one run of one
//! program, and a setting written down months ago must not quietly beat
//! what somebody typed a second ago.
//!
//! **Three states, not two** — [`Choice`], taken whole from the
//! desktop's model rather than reinvented. A file that says nothing
//! about a setting lets the next file down answer; a file that says
//! `Off` answers "nothing" and OUTRANKS what a system file names.
//! Clearing a setting therefore REMOVES the field, and writing `Off` is
//! a different act with a different meaning.
//!
//! Nothing here writes. The daemon reads its configuration and never
//! edits it: the file is the user's, and the one program in this family
//! that edits configuration by itself is the agent's toolbox, under
//! approval, on the desktop's file.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use nacelle_ai::backend::ollama;
use nacelle_ai::credentials::Env;
use nacelle_ai::tools::model::{Choice, Layered};
use nacelle_ai::tools::paths::{DesktopDirs, LEGACY_APP};
use nacelle_ai::Limits;
use serde::{Deserialize, Serialize};

use crate::media::{self, Ffmpeg};
use crate::proto::Wanted;

/// The daemon's file, in every configuration directory.
pub const CONF_RON: &str = "nacelle-ai.ron";

/// Whether a field is worth writing down. A field nothing was said
/// about is left out of the file entirely, which is what makes a
/// cleared setting indistinguishable from one that was never set.
fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

/// Everything `nacelle-ai.ron` can say.
///
/// Every field is defaulted, because RON parses all or nothing: a file
/// written against an older version of this struct, or half-written by
/// hand, still has to parse.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConf {
    /// What an `ask` that says `auto` resolves to: `auto`, `claude` or
    /// `local`. `local` is a PIN — see `backends`. This is `--backend`
    /// when nobody passes `--backend`, which on a desktop-started
    /// daemon is always.
    ///
    /// `Off` is not one of the answers: there is no "no backend", and
    /// `auto` is the neutral one. A file that says `Off` here is told
    /// so rather than read as something it did not say.
    #[serde(skip_serializing_if = "is_default")]
    pub backend: Choice,
    /// Which model to ask for. `Off` is the backend's own default said
    /// out loud — the first model the local server reports, or
    /// `claude-opus-4-8` — and it beats a system file that pins one.
    #[serde(skip_serializing_if = "is_default")]
    pub model: Choice,
    /// The Ollama server, in the shapes `OLLAMA_HOST` itself takes
    /// (`box:11434`, `:11434`, a full URL). `OLLAMA_HOST` outranks it.
    #[serde(skip_serializing_if = "is_default")]
    pub ollama_host: Choice,
    /// Where the socket goes, as a whole path to the socket FILE.
    ///
    /// Moving it is a decision about the far side too: the widgets
    /// compute `$XDG_RUNTIME_DIR/nacelle/ai.sock` from the spec page,
    /// and a daemon listening anywhere else is a daemon they will not
    /// find. Named here so a second daemon can be run beside the first
    /// on purpose, which is the one case where that is wanted.
    #[serde(skip_serializing_if = "is_default")]
    pub socket: Choice,
    /// The ffmpeg the `loop` tool execs. `NACELLE_AI_FFMPEG` outranks
    /// it; absent, both, the first executable `ffmpeg` on `PATH`
    /// answers.
    #[serde(skip_serializing_if = "is_default")]
    pub ffmpeg: Choice,
    #[serde(skip_serializing_if = "is_default")]
    pub limits: LimitsConf,
}

/// Where the agent loop gives up. Numbers, so [`Choice`] has nothing to
/// say about them: a number is either written down or it is not, and
/// there is no "no limit" to write.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitsConf {
    /// How many times the model may come back asking for another tool
    /// within one exchange. Built in: 16.
    #[serde(skip_serializing_if = "is_default")]
    pub max_turns: Option<u32>,
    /// Roughly how many bytes of conversation to keep. Built in:
    /// 200 000, about fifty thousand tokens of prose.
    #[serde(skip_serializing_if = "is_default")]
    pub history_bytes: Option<usize>,
}

impl Layered for LimitsConf {
    fn over(self, base: Self) -> Self {
        LimitsConf {
            max_turns: self.max_turns.over(base.max_turns),
            history_bytes: self.history_bytes.over(base.history_bytes),
        }
    }
}

impl Layered for DaemonConf {
    fn over(self, base: Self) -> Self {
        DaemonConf {
            backend: self.backend.over(base.backend),
            model: self.model.over(base.model),
            ollama_host: self.ollama_host.over(base.ollama_host),
            socket: self.socket.over(base.socket),
            ffmpeg: self.ffmpeg.over(base.ffmpeg),
            limits: self.limits.over(base.limits),
        }
    }
}

/// The configuration as the daemon will use it: every choice resolved,
/// every limit settled, and everything the file got wrong said out loud
/// rather than silently dropped.
///
/// A daemon has no window to put a notice in and the desktop starts it
/// without a terminal in front of anybody, so a wrong value that is
/// merely ignored is a wrong value nobody will ever find. [`notes`] is
/// what `main` prints to stderr, and what a test reads.
///
/// [`notes`]: Settled::notes
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settled {
    /// `None` = the file said nothing, so the built-in `auto` stands.
    pub backend: Option<Wanted>,
    pub model: Option<String>,
    pub ollama_host: Option<String>,
    pub socket: Option<PathBuf>,
    pub ffmpeg: Option<PathBuf>,
    pub limits: Limits,
    pub notes: Vec<String>,
}

impl DaemonConf {
    /// Resolve the document against the built-in defaults.
    pub fn settle(&self) -> Settled {
        let mut notes = Vec::new();

        let backend = match &self.backend {
            Choice::Inherit => None,
            Choice::Off => {
                notes.push(format!(
                    "{CONF_RON}: backend says Off, and there is no \"no backend\" \u{2014} it is \
                     auto, claude or local. Reading it as auto"
                ));
                None
            }
            Choice::Named(name) => match Wanted::of(name.trim()) {
                Some(wanted) => Some(wanted),
                None => {
                    notes.push(format!(
                        "{CONF_RON}: there is no backend called \"{name}\" \u{2014} it is auto, \
                         claude or local. Reading it as auto"
                    ));
                    None
                }
            },
        };

        // `Off` on any of these three is the built-in answer said out
        // loud: no pinned model, the default host, no named ffmpeg. It
        // differs from absence in the cascade and nowhere else, which
        // is why it lands on the same `None` here.
        let model = self.model.name().map(str::to_string);
        let ollama_host = self.ollama_host.name().map(str::to_string);

        let socket = self
            .socket
            .name()
            .and_then(|raw| match absolute(raw) {
                Some(path) => Some(path),
                None => {
                    notes.push(format!(
                        "{CONF_RON}: socket is \"{raw}\", which is not an absolute path \
                         \u{2014} a daemon has no working directory anybody can point at. \
                         Using the standard place"
                    ));
                    None
                }
            });

        let ffmpeg = self.ffmpeg.name().and_then(|raw| match absolute(raw) {
            Some(path) => Some(path),
            None => {
                notes.push(format!(
                    "{CONF_RON}: ffmpeg is \"{raw}\", which is not an absolute path \u{2014} \
                     name the program in full or leave the field out and let PATH answer. \
                     Ignoring it"
                ));
                None
            }
        });

        let built_in = Limits::default();
        let mut limits = built_in;
        match self.limits.max_turns {
            None => {}
            Some(0) => notes.push(format!(
                "{CONF_RON}: limits.max_turns is 0, which is an agent that cannot answer at \
                 all. Keeping {}",
                built_in.max_turns
            )),
            Some(turns) => limits.max_turns = turns,
        }
        match self.limits.history_bytes {
            None => {}
            Some(0) => notes.push(format!(
                "{CONF_RON}: limits.history_bytes is 0, which is a conversation with no room \
                 for the question. Keeping {}",
                built_in.history_bytes
            )),
            Some(bytes) => limits.history_bytes = bytes,
        }

        Settled {
            backend,
            model,
            ollama_host,
            socket,
            ffmpeg,
            limits,
            notes,
        }
    }

    /// The document as RON, the way this program would write one.
    /// Nothing writes the file — this is what `--print-config` shows.
    pub fn to_ron(&self) -> String {
        let pretty = ron::ser::PrettyConfig::new()
            .struct_names(false)
            .separate_tuple_members(false)
            .extensions(ron::extensions::Extensions::IMPLICIT_SOME);
        ron::Options::default()
            .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
            .to_string_pretty(self, pretty)
            .unwrap_or_else(|e| format!("// this document cannot be written out: {e}\n"))
    }
}

/// An absolute path, or nothing. A relative path in a daemon's
/// configuration is a path relative to whatever directory the thing
/// that started it happened to be in.
fn absolute(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    let path = Path::new(trimmed);
    path.is_absolute().then(|| path.to_path_buf())
}

/// One line of RON, parsed as a whole document.
pub fn parse(text: &str) -> Result<DaemonConf, String> {
    ron::Options::default()
        .with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
        .from_str::<DaemonConf>(text)
        .map_err(|e| e.to_string())
}

/// Every place the daemon looks for its file, most specific first.
///
/// Built from the same search path the toolbox uses for the desktop's
/// file, so the two programs agree about where the family's
/// configuration lives, with the folder's old name dropped: see the
/// module header for why this file is not looked for there.
pub fn places(env: &dyn Env) -> Vec<PathBuf> {
    DesktopDirs::from_env(env)
        .conf_levels()
        .into_iter()
        .map(|level| level.dir)
        .filter(|dir| dir.file_name() != Some(OsStr::new(LEGACY_APP)))
        .map(|dir| dir.join(CONF_RON))
        .collect()
}

/// What one rung of the cascade turned out to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rung {
    /// No file there. The ordinary case, and not worth a word.
    Missing,
    /// The file's text.
    Text(String),
    /// There is a file and it could not be read — a permission, a
    /// directory in its place. Said out loud: a setting that is not in
    /// force because nobody could open the file is not the same thing
    /// as a setting nobody wrote.
    Unreadable(String),
}

/// The cascade, folded, with a record of what was actually read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Loaded {
    pub conf: DaemonConf,
    /// The files that contributed, most specific first.
    pub read: Vec<PathBuf>,
    /// What went wrong on the way. Never fatal here: a broken system
    /// file must not stop a daemon the user's own file could have
    /// configured, and a daemon that refuses to start says even less
    /// than one that starts with the wrong settings.
    pub notes: Vec<String>,
}

/// Fold rungs given most specific first.
pub fn assemble(rungs: Vec<(PathBuf, Rung)>) -> Loaded {
    let mut out = Loaded::default();
    for (path, rung) in rungs {
        match rung {
            Rung::Missing => {}
            Rung::Unreadable(why) => out
                .notes
                .push(format!("{}: cannot be read ({why})", path.display())),
            Rung::Text(text) => match parse(&text) {
                // `self` is the more specific document, so what is
                // already folded stays on top of what comes next.
                Ok(doc) => {
                    out.conf = std::mem::take(&mut out.conf).over(doc);
                    out.read.push(path);
                }
                Err(why) => out.notes.push(format!(
                    "{}: this is not a readable {CONF_RON} ({why}) \u{2014} nothing in it is in \
                     force",
                    path.display()
                )),
            },
        }
    }
    out
}

/// Read one rung off the real filesystem.
fn rung_at(path: &Path) -> Rung {
    match fs::read_to_string(path) {
        Ok(text) => Rung::Text(text),
        Err(e) if e.kind() == ErrorKind::NotFound => Rung::Missing,
        Err(e) => Rung::Unreadable(e.to_string()),
    }
}

/// The whole cascade, from the real filesystem.
pub fn load(env: &dyn Env) -> Loaded {
    let rungs = places(env)
        .into_iter()
        .map(|path| {
            let rung = rung_at(&path);
            (path, rung)
        })
        .collect();
    assemble(rungs)
}

/// One named file and nothing else — `--config`.
///
/// Unforgiving where the cascade forgives: a file somebody named on the
/// command line and that is missing or broken is a mistake they made a
/// second ago and want to hear about, not a rung to skip.
pub fn load_named(path: &Path) -> Result<Loaded, String> {
    match rung_at(path) {
        Rung::Missing => Err(format!("{}: there is no such file", path.display())),
        Rung::Unreadable(why) => Err(format!("{}: cannot be read ({why})", path.display())),
        Rung::Text(text) => parse(&text)
            .map(|conf| Loaded {
                conf,
                read: vec![path.to_path_buf()],
                notes: Vec::new(),
            })
            .map_err(|why| format!("{}: {why}", path.display())),
    }
}

/// Which rung of the order decided a value that is in force.
///
/// Printed next to the value, because `claude` is a different answer
/// depending on whether a flag, a variable or a file said it — and the
/// three are edited in three different places.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// A flag somebody typed a second ago.
    Line,
    /// A variable exported for this run. It carries its own name: "the
    /// environment" is not somewhere a person can go and look.
    Env(&'static str),
    /// One of the files listed above the outcome.
    File,
    /// Nobody said anything, so this program's own answer stands.
    BuiltIn,
}

impl Source {
    /// The words `--print-config` puts in the brackets. A variable
    /// gives its own name, which is the only one of the four somebody
    /// can act on without reading this program's source.
    pub fn words(&self) -> &'static str {
        match self {
            Source::Line => "the command line",
            Source::Env(name) => name,
            Source::File => "the file",
            Source::BuiltIn => "this program's own",
        }
    }
}

/// One setting as it will actually be used, and which rung settled it.
#[derive(Clone, Debug, PartialEq)]
pub struct Said {
    /// What the daemon will use — or, when a rung named something
    /// unusable, the daemon's own sentence saying so.
    pub value: String,
    /// The rung that decided, or `None` when `value` is a complaint
    /// rather than a value.
    pub from: Option<Source>,
}

impl Said {
    pub fn value(value: impl Into<String>, from: Source) -> Said {
        Said {
            value: value.into(),
            from: Some(from),
        }
    }

    /// Something was named and cannot be used. The sentence is the same
    /// one the client would be shown, not a second wording of it.
    pub fn wrong(why: impl Into<String>) -> Said {
        Said {
            value: why.into(),
            from: None,
        }
    }
}

/// What the command line settled, and whether the command line is what
/// settled it.
///
/// `conf` cannot parse arguments and must not guess at them, so `main`
/// hands this in — it is the seam between the two, and the reason
/// `--print-config` can report a flag at all. Until 2026-08-18 it could
/// not: the report was handed the file's outcome alone and printed
/// `auto` at a daemon started with `--backend claude`.
#[derive(Clone, Debug, PartialEq)]
pub struct Chosen {
    pub backend: Wanted,
    pub backend_from_line: bool,
    pub model: Option<String>,
    pub model_from_line: bool,
}

/// Everything the daemon settled on: the file, with the command line
/// over it and the environment where the environment wins.
///
/// Built by [`in_force`] from THE SAME functions the daemon runs on —
/// `backends::ollama_at`, `Ffmpeg::pick`, `socket::place` — rather than
/// from a second reading of the file. A report that re-derives its
/// answers is a report that can be right about a program that no longer
/// exists.
#[derive(Clone, Debug, PartialEq)]
pub struct InForce {
    pub backend: Said,
    pub model: Said,
    pub ollama_host: Said,
    pub socket: Said,
    pub ffmpeg: Said,
    pub limits: Said,
}

/// Ask the machine what this daemon is going to run with.
///
/// Everything here goes through the daemon's own resolvers, so the two
/// cannot drift: the host through [`ollama_at`](crate::backends::ollama_at),
/// which also normalises it and drops a password somebody put in the
/// URL; ffmpeg through [`Ffmpeg::pick`](crate::media::Ffmpeg::pick),
/// which is where the environment beats the file; the socket through
/// [`socket::place`](crate::socket::place), so the report names the path
/// rather than the phrase "the standard place".
pub fn in_force(env: &dyn Env, settled: &Settled, chosen: &Chosen) -> InForce {
    let var = |name: &str| env.var(name);

    let backend = Said::value(
        format!("{:?}", chosen.backend).to_lowercase(),
        match (chosen.backend_from_line, settled.backend.is_some()) {
            (true, _) => Source::Line,
            (false, true) => Source::File,
            (false, false) => Source::BuiltIn,
        },
    );

    let model = match &chosen.model {
        Some(id) => Said::value(
            id.clone(),
            match chosen.model_from_line {
                true => Source::Line,
                false => Source::File,
            },
        ),
        None => Said::value("the backend's default", Source::BuiltIn),
    };

    let exported_host = env
        .var(ollama::HOST_VAR)
        .filter(|v| !v.trim().is_empty())
        .is_some();
    let ollama_host = Said::value(
        crate::backends::ollama_at(env, settled.ollama_host.as_deref())
            .host()
            .to_string(),
        match (exported_host, settled.ollama_host.is_some()) {
            (true, _) => Source::Env(ollama::HOST_VAR),
            (false, true) => Source::File,
            (false, false) => Source::BuiltIn,
        },
    );

    let socket = match &settled.socket {
        Some(path) => Said::value(path.display().to_string(), Source::File),
        None => match crate::socket::place(&var) {
            Ok(dir) => Said::value(
                dir.join(crate::socket::SOCKET_NAME).display().to_string(),
                match var(crate::socket::RUNTIME_DIR_ENV)
                    .filter(|v| !v.trim().is_empty())
                    .is_some()
                {
                    true => Source::Env(crate::socket::RUNTIME_DIR_ENV),
                    false => Source::BuiltIn,
                },
            ),
            Err(why) => Said::wrong(why),
        },
    };

    let ffmpeg = match Ffmpeg::pick(&var, settled.ffmpeg.as_deref()) {
        Ok(found) => Said::value(
            found.program().display().to_string(),
            match (
                var(media::FFMPEG_ENV)
                    .filter(|v| !v.trim().is_empty())
                    .is_some(),
                settled.ffmpeg.is_some(),
            ) {
                (true, _) => Source::Env(media::FFMPEG_ENV),
                (false, true) => Source::File,
                (false, false) => Source::Env("PATH"),
            },
        ),
        Err(why) => Said::wrong(why),
    };

    let built_in = Limits::default();
    let limits = Said::value(
        format!(
            "{} turns, {} bytes of history",
            settled.limits.max_turns, settled.limits.history_bytes
        ),
        match settled.limits == built_in {
            true => Source::BuiltIn,
            false => Source::File,
        },
    );

    InForce {
        backend,
        model,
        ollama_host,
        socket,
        ffmpeg,
        limits,
    }
}

/// What `--print-config` writes: where it looked, what the files said,
/// and what the daemon will actually do with it. A daemon with no window
/// and no log of its own otherwise has no way to answer "what do you
/// think your settings are".
///
/// The document and the outcome are printed separately on purpose. They
/// differ exactly where something is wrong — a backend nobody has, a
/// limit of zero — and printing only the document would hide that,
/// while printing only the outcome would hide which file to go and fix.
///
/// The outcome half is [`InForce`] and nothing else. It must not be
/// re-derived here from `settled`: `settled` is the FILE's answer, and
/// the two rungs above it — a flag and an exported variable — are
/// exactly the ones somebody runs this for.
pub fn report(loaded: &Loaded, settled: &Settled, force: &InForce, places: &[PathBuf]) -> String {
    let mut out = String::new();
    out.push_str("// nacelle-ai, the configuration in force\n");
    out.push_str("//\n// looked in, most specific first:\n");
    for place in places {
        let mark = if loaded.read.contains(place) {
            "read"
        } else {
            "not there"
        };
        let _ = writeln!(out, "//   {} \u{2014} {mark}", place.display());
    }
    if loaded.read.is_empty() {
        out.push_str("//\n// no file was read: the outcome below is the command line, the \
                      environment and this program's own answers.\n");
    }
    out.push_str("//\n// what the daemon will do with it:\n");
    let mut line = |name: &str, said: &Said| {
        let _ = match &said.from {
            Some(from) => writeln!(out, "//   {name:<13} {} ({})", said.value, from.words()),
            None => writeln!(out, "//   {name:<13} ! {}", said.value),
        };
    };
    line("backend", &force.backend);
    line("model", &force.model);
    line("ollama host", &force.ollama_host);
    line("socket", &force.socket);
    line("ffmpeg", &force.ffmpeg);
    line("limits", &force.limits);
    for note in &settled.notes {
        let _ = writeln!(out, "//   ! {note}");
    }
    out.push_str("//\n// what the files say:\n");
    out.push_str(&loaded.conf.to_ron());
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_document_settles_to_the_built_in_limits() {
        let settled = DaemonConf::default().settle();
        assert_eq!(settled.limits, Limits::default());
        assert_eq!(settled.backend, None);
        assert!(settled.notes.is_empty(), "said: {:?}", settled.notes);
    }

    #[test]
    fn the_user_file_wins_field_by_field_over_the_system_one() {
        let user = "(backend: Named(\"local\"))";
        let system = "(backend: Named(\"claude\"), model: Named(\"llama3\"), \
                      limits: (max_turns: 4))";
        let loaded = assemble(vec![
            (PathBuf::from("/u/nacelle-ai.ron"), Rung::Text(user.into())),
            (PathBuf::from("/etc/xdg/nacelle/nacelle-ai.ron"), Rung::Text(system.into())),
        ]);
        assert!(loaded.notes.is_empty(), "said: {:?}", loaded.notes);
        let settled = loaded.conf.settle();
        // The user's backend answers, and the system's model still does
        // — the cascade is per field, not per file.
        assert_eq!(settled.backend, Some(Wanted::Local));
        assert_eq!(settled.model.as_deref(), Some("llama3"));
        assert_eq!(settled.limits.max_turns, 4);
    }

    #[test]
    fn off_outranks_a_system_file_that_names_one() {
        let loaded = assemble(vec![
            (PathBuf::from("/u"), Rung::Text("(model: Off)".into())),
            (PathBuf::from("/s"), Rung::Text("(model: Named(\"llama3\"))".into())),
        ]);
        assert_eq!(loaded.conf.model, Choice::Off);
        // Off and absent resolve to the same run-time answer — the
        // backend's own default — and differ only in the cascade.
        assert_eq!(loaded.conf.settle().model, None);
    }

    #[test]
    fn a_broken_file_is_said_out_loud_and_the_rest_still_answers() {
        let loaded = assemble(vec![
            (PathBuf::from("/u/nacelle-ai.ron"), Rung::Text("(backend: {".into())),
            (PathBuf::from("/s/nacelle-ai.ron"), Rung::Text("(model: Named(\"m\"))".into())),
        ]);
        assert_eq!(loaded.notes.len(), 1, "said: {:?}", loaded.notes);
        assert!(loaded.notes[0].contains("/u/nacelle-ai.ron"), "said: {:?}", loaded.notes);
        assert_eq!(loaded.conf.model, Choice::named("m"));
        assert_eq!(loaded.read, vec![PathBuf::from("/s/nacelle-ai.ron")]);
    }

    #[test]
    fn an_unknown_backend_is_named_in_a_note_rather_than_guessed_at() {
        let conf = parse("(backend: Named(\"gpt\"))").expect("this parses");
        let settled = conf.settle();
        assert_eq!(settled.backend, None);
        assert_eq!(settled.notes.len(), 1);
        assert!(settled.notes[0].contains("gpt"), "said: {:?}", settled.notes);
    }

    #[test]
    fn off_is_not_a_backend_and_says_so() {
        let settled = parse("(backend: Off)").expect("this parses").settle();
        assert_eq!(settled.backend, None);
        assert!(
            settled.notes.iter().any(|n| n.contains("no backend")),
            "said: {:?}",
            settled.notes
        );
    }

    #[test]
    fn a_relative_socket_or_ffmpeg_is_refused_with_a_reason() {
        let conf = parse("(socket: Named(\"ai.sock\"), ffmpeg: Named(\"ffmpeg\"))")
            .expect("this parses");
        let settled = conf.settle();
        assert_eq!(settled.socket, None);
        assert_eq!(settled.ffmpeg, None);
        assert_eq!(settled.notes.len(), 2, "said: {:?}", settled.notes);
    }

    #[test]
    fn a_zero_limit_keeps_the_built_in_one_and_says_so() {
        let settled = parse("(limits: (max_turns: 0, history_bytes: 0))")
            .expect("this parses")
            .settle();
        assert_eq!(settled.limits, Limits::default());
        assert_eq!(settled.notes.len(), 2, "said: {:?}", settled.notes);
    }

    #[test]
    fn an_incomplete_document_still_parses() {
        // RON parses all or nothing, so this is the property that keeps
        // a file written against an older version of the struct alive.
        let conf = parse("(model: Named(\"llama3\"))").expect("this parses");
        assert_eq!(conf.model, Choice::named("llama3"));
        assert_eq!(conf.backend, Choice::Inherit);
    }

    #[test]
    fn a_settled_document_writes_out_only_what_was_said() {
        let conf = parse("(model: Named(\"llama3\"))").expect("this parses");
        let text = conf.to_ron();
        assert!(text.contains("llama3"), "wrote: {text}");
        assert!(!text.contains("backend"), "wrote: {text}");
        // And what it writes reads back as the same document.
        assert_eq!(parse(&text).expect("this parses"), conf);
    }

    /// The example in `docs/` is the only description of this file
    /// anybody will read before writing one, so it has to be a document
    /// this parser accepts. It lives beside the source, so this reads a
    /// path relative to the crate rather than to a working directory.
    #[test]
    fn the_shipped_example_is_a_document_this_parser_accepts() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/nacelle-ai.ron.example");
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let conf = parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        // Every line of it is commented out, which is the point: a
        // machine that copies it verbatim has changed nothing.
        assert_eq!(conf, DaemonConf::default());
        assert!(conf.settle().notes.is_empty());
    }

    #[test]
    fn the_old_folder_name_is_not_a_place_this_file_is_looked_for() {
        struct Fixed;
        impl Env for Fixed {
            fn var(&self, key: &str) -> Option<String> {
                match key {
                    "XDG_CONFIG_HOME" => Some("/home/u/.config".to_string()),
                    "XDG_CONFIG_DIRS" => Some("/etc/xdg".to_string()),
                    _ => None,
                }
            }
        }
        let places = places(&Fixed);
        assert_eq!(
            places,
            vec![
                PathBuf::from("/home/u/.config/nacelle/nacelle-ai.ron"),
                PathBuf::from("/etc/xdg/nacelle/nacelle-ai.ron"),
            ]
        );
    }

    // -----------------------------------------------------------------
    // What `--print-config` reports.
    //
    // The report had no test at all until 2026-08-18, and it was wrong:
    // it printed the FILE's answer under a heading that says "what the
    // daemon will do with it", so a daemon started `--backend claude`
    // reported `auto` and one running against an exported OLLAMA_HOST
    // reported the host in the file. Every test below is about a rung
    // ABOVE the file, because that is the whole of what was missing.

    /// An environment that is a table, so a developer's own exported
    /// OLLAMA_HOST cannot decide whether these pass.
    struct Table(&'static [(&'static str, &'static str)]);

    impl Env for Table {
        fn var(&self, key: &str) -> Option<String> {
            self.0
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_string())
        }
    }

    fn from_file(text: &str) -> Settled {
        parse(text).expect("this parses").settle()
    }

    fn line(text: &str) -> Chosen {
        let settled = from_file(text);
        Chosen {
            backend: settled.backend.unwrap_or_default(),
            backend_from_line: false,
            model: settled.model,
            model_from_line: false,
        }
    }

    /// The flag, not the file — the finding this whole section exists
    /// for. A file saying `local`/`llama3` and a command line saying
    /// `claude`/`my-model` is a daemon that runs claude/my-model, and
    /// the report said `local`/`llama3`.
    #[test]
    fn the_report_says_what_the_command_line_chose() {
        let settled = from_file("(backend: Named(\"local\"), model: Named(\"llama3\"))");
        let chosen = Chosen {
            backend: Wanted::Claude,
            backend_from_line: true,
            model: Some("my-model".to_string()),
            model_from_line: true,
        };
        let force = in_force(&Table(&[]), &settled, &chosen);
        assert_eq!(force.backend, Said::value("claude", Source::Line));
        assert_eq!(force.model, Said::value("my-model", Source::Line));

        let printed = report(&Loaded::default(), &settled, &force, &[]);
        assert!(printed.contains("backend       claude (the command line)"), "{printed}");
        assert!(printed.contains("model         my-model (the command line)"), "{printed}");
        assert!(!printed.contains("backend       local"), "{printed}");
    }

    /// And when no flag was passed, the file gets the credit.
    #[test]
    fn the_report_says_which_rung_answered() {
        let text = "(backend: Named(\"local\"), model: Named(\"llama3\"))";
        let settled = from_file(text);
        let force = in_force(&Table(&[]), &settled, &line(text));
        assert_eq!(force.backend, Said::value("local", Source::File));
        assert_eq!(force.model, Said::value("llama3", Source::File));

        let bare = in_force(&Table(&[]), &Settled::default(), &line("()"));
        assert_eq!(bare.backend, Said::value("auto", Source::BuiltIn));
        assert_eq!(
            bare.model,
            Said::value("the backend's default", Source::BuiltIn)
        );
    }

    /// The exported variable, not the file. The daemon asks the
    /// exported host — `backends::ollama_at` is the one rule and this
    /// goes through it — and the report used to name the other one.
    #[test]
    fn the_report_says_the_host_the_daemon_will_ask() {
        let settled = from_file("(ollama_host: Named(\"127.0.0.1:59999\"))");
        let exported = Table(&[("OLLAMA_HOST", "http://localhost:11434")]);
        let force = in_force(&exported, &settled, &line("()"));
        assert_eq!(
            force.ollama_host,
            Said::value("http://localhost:11434", Source::Env("OLLAMA_HOST"))
        );

        // Nothing exported: the file answers, normalised the way the
        // request will actually be addressed.
        let force = in_force(&Table(&[]), &settled, &line("()"));
        assert_eq!(
            force.ollama_host,
            Said::value("http://127.0.0.1:59999", Source::File)
        );
    }

    /// Going through the daemon's own resolver has a second effect the
    /// old report did not have: `normalise_host` drops a password, and
    /// the report printed the file's string raw.
    #[test]
    fn a_password_written_into_the_host_is_not_printed() {
        let settled = from_file("(ollama_host: Named(\"http://u:hunter2@box:11434\"))");
        let force = in_force(&Table(&[]), &settled, &line("()"));
        assert!(!force.ollama_host.value.contains("hunter2"), "{:?}", force.ollama_host);
        assert!(force.ollama_host.value.contains("box:11434"), "{:?}", force.ollama_host);
    }

    /// The same order, for the program the `loop` tool execs. Both
    /// paths here are on every Linux the daemon runs on.
    #[test]
    fn the_report_says_the_ffmpeg_the_loop_tool_will_exec() {
        let settled = from_file("(ffmpeg: Named(\"/bin/cat\"))");
        let exported = Table(&[("NACELLE_AI_FFMPEG", "/bin/sh")]);
        assert_eq!(
            in_force(&exported, &settled, &line("()")).ffmpeg,
            Said::value("/bin/sh", Source::Env("NACELLE_AI_FFMPEG"))
        );
        assert_eq!(
            in_force(&Table(&[]), &settled, &line("()")).ffmpeg,
            Said::value("/bin/cat", Source::File)
        );
    }

    /// A rung that named something unusable is a complaint, not a
    /// value, and the report marks it as one. The sentence is the
    /// daemon's own — the same one the client is shown.
    #[test]
    fn an_ffmpeg_that_cannot_be_run_is_reported_as_a_complaint() {
        let settled = from_file("(ffmpeg: Named(\"/nonexistent/ffmpeg\"))");
        let force = in_force(&Table(&[("PATH", "")]), &settled, &line("()"));
        assert_eq!(force.ffmpeg.from, None);
        assert!(force.ffmpeg.value.contains("not an executable file"), "{:?}", force.ffmpeg);
        let printed = report(&Loaded::default(), &settled, &force, &[]);
        assert!(printed.contains("ffmpeg        ! "), "{printed}");
    }

    #[test]
    fn with_no_ffmpeg_anywhere_the_report_says_that_rather_than_a_rule() {
        let force = in_force(&Table(&[("PATH", "")]), &Settled::default(), &line("()"));
        assert_eq!(force.ffmpeg.from, None);
        assert!(force.ffmpeg.value.contains("not installed"), "{:?}", force.ffmpeg);
    }

    /// "the standard place" is not an answer somebody can check against
    /// the machine in front of them; the path is.
    #[test]
    fn the_report_names_the_socket_rather_than_the_phrase() {
        let session = Table(&[("XDG_RUNTIME_DIR", "/run/user/1000")]);
        let force = in_force(&session, &Settled::default(), &line("()"));
        assert_eq!(
            force.socket,
            Said::value("/run/user/1000/nacelle/ai.sock", Source::Env("XDG_RUNTIME_DIR"))
        );
        let named = from_file("(socket: Named(\"/tmp/two/ai.sock\"))");
        assert_eq!(
            in_force(&session, &named, &line("()")).socket,
            Said::value("/tmp/two/ai.sock", Source::File)
        );
    }

    /// The other half of the page is untouched: the document is still
    /// printed whole, because the outcome alone would hide which file
    /// to go and edit.
    #[test]
    fn the_document_is_still_printed_beside_the_outcome() {
        let text = "(backend: Named(\"local\"))";
        let settled = from_file(text);
        let loaded = assemble(vec![(
            PathBuf::from("/u/nacelle-ai.ron"),
            Rung::Text(text.to_string()),
        )]);
        let force = in_force(&Table(&[]), &settled, &line(text));
        let printed = report(&loaded, &settled, &force, &[PathBuf::from("/u/nacelle-ai.ron")]);
        assert!(printed.contains("/u/nacelle-ai.ron \u{2014} read"), "{printed}");
        assert!(printed.contains("what the files say:"), "{printed}");
        assert!(printed.contains("backend: Named(\"local\")"), "{printed}");
    }

    /// A wrong value in the file still reaches the page, under the
    /// outcome where it belongs.
    #[test]
    fn what_the_file_got_wrong_is_still_said_out_loud() {
        let settled = from_file("(backend: Named(\"gpt\"))");
        let force = in_force(&Table(&[]), &settled, &line("(backend: Named(\"gpt\"))"));
        let printed = report(&Loaded::default(), &settled, &force, &[]);
        assert!(printed.contains("there is no backend called \"gpt\""), "{printed}");
        // and the daemon runs auto, which is what the report says.
        assert_eq!(force.backend, Said::value("auto", Source::BuiltIn));
    }
}
