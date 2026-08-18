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

use nacelle_ai::credentials::Env;
use nacelle_ai::tools::model::{Choice, Layered};
use nacelle_ai::tools::paths::{DesktopDirs, LEGACY_APP};
use nacelle_ai::Limits;
use serde::{Deserialize, Serialize};

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

/// What `--print-config` writes: where it looked, what the files said,
/// and what the daemon will actually do with it. A daemon with no window
/// and no log of its own otherwise has no way to answer "what do you
/// think your settings are".
///
/// The document and the outcome are printed separately on purpose. They
/// differ exactly where something is wrong — a backend nobody has, a
/// limit of zero — and printing only the document would hide that,
/// while printing only the outcome would hide which file to go and fix.
pub fn report(loaded: &Loaded, settled: &Settled, places: &[PathBuf]) -> String {
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
        out.push_str("//\n// nothing was read: every value below is this program's own.\n");
    }
    out.push_str("//\n// what the daemon will do with it:\n");
    let said = |value: Option<String>, built_in: &str| match value {
        Some(v) => v,
        None => format!("{built_in} (this program's own)"),
    };
    let _ = writeln!(
        out,
        "//   backend       {}",
        said(settled.backend.map(|b| format!("{b:?}").to_lowercase()), "auto")
    );
    let _ = writeln!(
        out,
        "//   model         {}",
        said(settled.model.clone(), "the backend's default")
    );
    let _ = writeln!(
        out,
        "//   ollama host   {}",
        said(settled.ollama_host.clone(), "OLLAMA_HOST, or localhost:11434")
    );
    let _ = writeln!(
        out,
        "//   socket        {}",
        said(
            settled.socket.as_ref().map(|p| p.display().to_string()),
            "the standard place"
        )
    );
    let _ = writeln!(
        out,
        "//   ffmpeg        {}",
        said(
            settled.ffmpeg.as_ref().map(|p| p.display().to_string()),
            "NACELLE_AI_FFMPEG, or the first on PATH"
        )
    );
    let _ = writeln!(
        out,
        "//   limits        {} turns, {} bytes of history",
        settled.limits.max_turns, settled.limits.history_bytes
    );
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
}
