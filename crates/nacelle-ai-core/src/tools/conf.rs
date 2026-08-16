//! `nacelle-desktop.ron`: how it is read, which settings this agent may
//! change, and how exactly one of them is changed.
//!
//! The format is RON — the owner's decision of 2026-08-12, see
//! `.gap-program/decyzja-konfiguracja-ron.md` — read and written through
//! the typed document in [`model`](crate::tools::model), which mirrors
//! the desktop's own. The document is read as a CASCADE: the user's
//! file laid over the system ones, FIELD by field, most specific first.
//! A field the user never set is answered by a system file, which is
//! how a distribution ships a default without copying anything into a
//! home directory. Writes go to the user's own
//! `nacelle/nacelle-desktop.ron` and nowhere else.
//!
//! The `Key=Value` file that came before (`nacelle-desktop.conf`) is
//! still READ wherever no `.ron` stands beside it — per directory, so a
//! distribution still shipping the old format is answered by a user who
//! has already moved on. It is never written and never deleted: the
//! first write SEEDS the new document from the user's own old file, so
//! nothing the user had set is lost, and the old file stays byte for
//! byte as it was.
//!
//! [`KEYS`] is a closed list on purpose. A tool that would write any
//! field at all is a tool that lets a model quietly fill a user's
//! configuration with things the desktop has never heard of. Refusing
//! an unknown key and naming the ones that exist is the only version of
//! this the model can learn from. The key names are the OLD format's
//! spellings (`Theme`, `SoundVolume`, …) — they are the tool's public
//! vocabulary, and changing them would re-teach every client for no
//! gain; each one maps onto a field of the typed document below.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::redact::deny::Denylist;
use crate::tools::error::ToolError;
use crate::tools::model::{Choice, DesktopConf, Layered};
use crate::tools::paths::{confine, is_identifier, safe_component, ConfLevel, DesktopDirs};
use crate::tools::write::{self, Replaced};

/// What a key's value may look like.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accepts {
    /// A single path component: the name of something installed, never
    /// a path. Joined into a data directory by the desktop, which is
    /// why `..` and separators are refused here.
    Name,
    /// Letters, digits, `_` and `-`. A theme is resolved by name and a
    /// name that could be a path would be a file-read primitive.
    Identifier,
    /// Free text — a font family, say, which may well contain spaces.
    Text,
    /// A whole number, within the range the desktop itself honours.
    Number { min: u32, max: u32 },
    /// `0` or `1`.
    Flag,
    /// One of a fixed set of words.
    Word(&'static [&'static str]),
}

/// One setting the desktop reads, under the name this tool offers it as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfKey {
    pub name: &'static str,
    pub accepts: Accepts,
    /// What the key does, in the words the model will pass on to the
    /// user.
    pub summary: &'static str,
}

/// The colour spaces the desktop offers, in its own order.
const COLOR_SPACES: &[&str] = &[
    "auto",
    "srgb",
    "display p3",
    "adobe rgb",
    "bt2020 pq",
    "bt2020 hlg",
    "scrgb linear",
];

/// The bit depths the swapchain can be asked for.
const COLOR_DEPTHS: &[&str] = &["8", "10", "12", "16"];

/// Every setting this agent may change, in the desktop's own grouping:
/// appearance, fonts, sound, colour, glass, grid.
pub const KEYS: &[ConfKey] = &[
    ConfKey {
        name: "Theme",
        accepts: Accepts::Identifier,
        summary: "the theme the desktop loads; empty removes the setting and leaves the \
                  cascade's answer, or the toolkit's built-in master theme",
    },
    ConfKey {
        name: "Layaut",
        accepts: Accepts::Name,
        summary: "the layaut (panel layout) the desktop arranges itself with",
    },
    ConfKey {
        name: "Sounds",
        accepts: Accepts::Name,
        summary: "the installed sound set the interface plays from",
    },
    ConfKey {
        name: "TermFontSize",
        accepts: Accepts::Number { min: 50, max: 200 },
        summary: "terminal font size, percent of the default",
    },
    ConfKey {
        name: "TermFontFamily",
        accepts: Accepts::Text,
        summary: "terminal font family",
    },
    ConfKey {
        name: "TermFontWeight",
        accepts: Accepts::Text,
        summary: "terminal font weight",
    },
    ConfKey {
        name: "UIFontSize",
        accepts: Accepts::Number { min: 30, max: 125 },
        summary: "interface font size, percent of the default",
    },
    ConfKey {
        name: "UIFontFamily",
        accepts: Accepts::Text,
        summary: "interface font family",
    },
    ConfKey {
        name: "UIFontWeight",
        accepts: Accepts::Text,
        summary: "interface font weight",
    },
    ConfKey {
        name: "SoundVolume",
        accepts: Accepts::Number { min: 0, max: 100 },
        summary: "master volume of the interface sounds, percent",
    },
    ConfKey {
        name: "SoundTyping",
        accepts: Accepts::Flag,
        summary: "typing sounds on (1) or off (0)",
    },
    ConfKey {
        name: "SoundAmbient",
        accepts: Accepts::Flag,
        summary: "the ambient sound bed on (1) or off (0)",
    },
    ConfKey {
        name: "ColorDepth",
        accepts: Accepts::Word(COLOR_DEPTHS),
        summary: "bit depth the swapchain is asked for; a Wayland-session setting",
    },
    ConfKey {
        name: "ColorSpace",
        accepts: Accepts::Word(COLOR_SPACES),
        summary: "colour space the compositor is asked to show the window in",
    },
    ConfKey {
        name: "ColorLut",
        accepts: Accepts::Name,
        summary: "file name of a grading LUT installed under the data directory's lut/; \
                  empty is an explicit \"no LUT\" that outranks a system default",
    },
    ConfKey {
        name: "ColorIcc",
        accepts: Accepts::Name,
        summary: "file name of an ICC profile installed under the data directory's icc/; \
                  empty is an explicit \"no profile\" that outranks a system default",
    },
    ConfKey {
        name: "BlurRadius",
        accepts: Accepts::Number { min: 0, max: 100 },
        summary: "how deep the frosted-glass blur goes, percent",
    },
    ConfKey {
        name: "BlurOpacity",
        accepts: Accepts::Number { min: 0, max: 100 },
        summary: "opacity of the glass tint, percent; below 100 the boards beneath show through",
    },
    ConfKey {
        name: "GridSnap",
        accepts: Accepts::Flag,
        summary: "the layout editor's snap-to-grid on (1) or off (0)",
    },
    ConfKey {
        name: "GridCols",
        accepts: Accepts::Number { min: 15, max: 100 },
        summary: "columns in the layout editor's grid",
    },
    ConfKey {
        name: "GridRows",
        accepts: Accepts::Number { min: 15, max: 100 },
        summary: "rows in the layout editor's grid",
    },
    ConfKey {
        name: "GridPadding",
        accepts: Accepts::Number { min: 0, max: 40 },
        summary: "padding around a widget in the layout editor, pixels",
    },
];

/// The key of that name, or nothing.
pub fn key(name: &str) -> Option<&'static ConfKey> {
    KEYS.iter().find(|k| k.name == name)
}

impl Accepts {
    /// One line a model can read to know what to send.
    pub fn describe(&self) -> String {
        match self {
            Accepts::Name => {
                "the name of an installed item — one path component, no '/' and no '..'".into()
            }
            Accepts::Identifier => "a name of letters, digits, '_' and '-'".into(),
            Accepts::Text => "free text on one line".into(),
            Accepts::Number { min, max } => format!("a whole number from {min} to {max}"),
            Accepts::Flag => "1 or 0".into(),
            Accepts::Word(words) => format!("one of: {}", words.join(", ")),
        }
    }
}

impl ConfKey {
    /// The value this key would be set to, or why it may not be.
    ///
    /// Runs before anything on disk is touched. The empty string is
    /// accepted for every key: it clears the setting — by removing the
    /// field, or, for the two controls that offer "none" (the LUT and
    /// the ICC profile), by writing an explicit off.
    pub fn check(&self, value: &str) -> Result<String, ToolError> {
        let value = value.trim();
        // Line breaks cannot smuggle a second setting into a document
        // whose writer serialises a typed value — but a control
        // character in a name is still nothing the desktop would ever
        // honour, so it is refused before every other check.
        if value.contains('\n') || value.contains('\r') || value.contains('\0') {
            return Err(ToolError::Rejected {
                reason: format!(
                    "the value for {} contains a line break or a NUL, which no setting takes",
                    self.name
                ),
            });
        }
        if value.is_empty() {
            return Ok(String::new());
        }
        let rejected = |reason: String| ToolError::Rejected { reason };
        match self.accepts {
            Accepts::Name => safe_component(value).ok_or_else(|| {
                rejected(format!(
                    "\"{value}\" is not a name: {} takes {}",
                    self.name,
                    self.accepts.describe()
                ))
            }),
            Accepts::Identifier => {
                if is_identifier(value) {
                    Ok(value.to_string())
                } else {
                    Err(rejected(format!(
                        "\"{value}\" is not a name: {} takes {}",
                        self.name,
                        self.accepts.describe()
                    )))
                }
            }
            Accepts::Text => Ok(value.to_string()),
            Accepts::Number { min, max } => match value.parse::<u32>() {
                Ok(n) if n >= min && n <= max => Ok(n.to_string()),
                _ => Err(rejected(format!(
                    "{} takes {}, not \"{value}\"",
                    self.name,
                    self.accepts.describe()
                ))),
            },
            Accepts::Flag => match value {
                "0" | "1" => Ok(value.to_string()),
                _ => Err(rejected(format!(
                    "{} takes {}, not \"{value}\"",
                    self.name,
                    self.accepts.describe()
                ))),
            },
            Accepts::Word(words) => {
                // The desktop lower-cases these before matching, so a
                // capitalised answer from a model is right and is
                // stored the way the desktop will read it.
                let lower = value.to_lowercase();
                if words.contains(&lower.as_str()) {
                    Ok(lower)
                } else {
                    Err(rejected(format!(
                        "{} takes {}, not \"{value}\"",
                        self.name,
                        self.accepts.describe()
                    )))
                }
            }
        }
    }
}

// ---------------------------------------------------------- the mapping

/// The checked value, put into its field of the document.
///
/// `value` has been through [`ConfKey::check`], so parsing here cannot
/// fail on shape — the `unwrap_or_default` arms are unreachable and
/// harmless. An empty value clears: `Choice::named("")` is `Inherit`
/// (the field is removed and the cascade answers again), `None` removes
/// a number or a switch, and the two offable names — the LUT and the
/// ICC profile — become an explicit `Off` that outranks a system file.
fn apply(doc: &mut DesktopConf, key: &ConfKey, value: &str) {
    let named = || Choice::named(value);
    let offable = || {
        if value.is_empty() {
            Choice::Off
        } else {
            Choice::Named(value.to_string())
        }
    };
    let num = || {
        if value.is_empty() {
            None
        } else {
            value.parse::<u32>().ok()
        }
    };
    let size = || {
        if value.is_empty() {
            None
        } else {
            value.parse::<f32>().ok()
        }
    };
    let flag = || match value {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    };
    match key.name {
        "Theme" => doc.theme = named(),
        "Layaut" => doc.layaut = named(),
        "Sounds" => doc.sounds = named(),
        "TermFontSize" => doc.term_font.size = size(),
        "TermFontFamily" => doc.term_font.family = named(),
        "TermFontWeight" => doc.term_font.weight = named(),
        "UIFontSize" => doc.ui_font.size = size(),
        "UIFontFamily" => doc.ui_font.family = named(),
        "UIFontWeight" => doc.ui_font.weight = named(),
        "SoundVolume" => doc.sound.volume = num(),
        "SoundTyping" => doc.sound.typing = flag(),
        "SoundAmbient" => doc.sound.ambient = flag(),
        "ColorDepth" => doc.color.depth = num(),
        "ColorSpace" => doc.color.space = named(),
        "ColorLut" => doc.color.lut = offable(),
        "ColorIcc" => doc.color.icc = offable(),
        "BlurRadius" => doc.blur.radius = num(),
        "BlurOpacity" => doc.blur.opacity = num(),
        "GridSnap" => doc.grid.snap = flag(),
        "GridCols" => doc.grid.cols = num(),
        "GridRows" => doc.grid.rows = num(),
        "GridPadding" => doc.grid.padding = num(),
        // A key in KEYS with no arm here is a bug this module owns.
        other => unreachable!("no field mapping for configuration key {other}"),
    }
}

/// What one document says about one key, as text — or `None` when it
/// says nothing and the next rung of the cascade answers.
///
/// `Off` renders as the empty string: it IS an answer ("nothing,
/// explicitly"), which is exactly how the old format spelled it.
pub fn field_value(doc: &DesktopConf, key_name: &str) -> Option<String> {
    fn choice(c: &Choice) -> Option<String> {
        match c {
            Choice::Inherit => None,
            Choice::Off => Some(String::new()),
            Choice::Named(n) => Some(n.clone()),
        }
    }
    fn num(n: &Option<u32>) -> Option<String> {
        n.map(|n| n.to_string())
    }
    fn size(n: &Option<f32>) -> Option<String> {
        n.map(|n| format!("{n}"))
    }
    fn flag(b: &Option<bool>) -> Option<String> {
        b.map(|b| if b { "1" } else { "0" }.to_string())
    }
    match key_name {
        "Theme" => choice(&doc.theme),
        "Layaut" => choice(&doc.layaut),
        "Sounds" => choice(&doc.sounds),
        "TermFontSize" => size(&doc.term_font.size),
        "TermFontFamily" => choice(&doc.term_font.family),
        "TermFontWeight" => choice(&doc.term_font.weight),
        "UIFontSize" => size(&doc.ui_font.size),
        "UIFontFamily" => choice(&doc.ui_font.family),
        "UIFontWeight" => choice(&doc.ui_font.weight),
        "SoundVolume" => num(&doc.sound.volume),
        "SoundTyping" => flag(&doc.sound.typing),
        "SoundAmbient" => flag(&doc.sound.ambient),
        "ColorDepth" => num(&doc.color.depth),
        "ColorSpace" => choice(&doc.color.space),
        "ColorLut" => choice(&doc.color.lut),
        "ColorIcc" => choice(&doc.color.icc),
        "BlurRadius" => num(&doc.blur.radius),
        "BlurOpacity" => num(&doc.blur.opacity),
        "GridSnap" => flag(&doc.grid.snap),
        "GridCols" => num(&doc.grid.cols),
        "GridRows" => num(&doc.grid.rows),
        "GridPadding" => num(&doc.grid.padding),
        _ => None,
    }
}

// ------------------------------------------------------------- reading

/// One effective setting, and the file it won in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setting {
    pub key: String,
    pub value: String,
    pub from: PathBuf,
}

/// What a read of the whole cascade found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Effective {
    /// Every key of [`KEYS`] that any file answers, sorted by key.
    pub settings: Vec<Setting>,
    /// Files that exist and could not be read, each with the sentence
    /// that says why. A broken file is skipped WHOLE — every setting in
    /// it counts as unset — and that has to be said rather than shown
    /// as a mysteriously missing value.
    pub problems: Vec<String>,
}

/// Every `Key=Value` in the old format's text, in file order, keys and
/// values trimmed. Comments and blank lines contribute nothing.
pub fn parse(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    out
}

/// The old format's text as the map [`DesktopConf::from_legacy`] reads.
/// Insertion order makes the LAST declaration of a key win, exactly as
/// the desktop's old reader did.
fn legacy_map(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (k, v) in parse(text) {
        map.insert(k, v);
    }
    map
}

/// A RON document, parsed the way the desktop parses it.
///
/// `implicit_some` is on so a person can write `volume: 80` where the
/// type says `Option<u32>` — a configuration file is written by hand at
/// least as often as by a program.
pub fn parse_ron(text: &str) -> Result<DesktopConf, String> {
    ron_options()
        .from_str::<DesktopConf>(text)
        .map_err(|e| format!("line {}, column {}: {}", e.span.start.line, e.span.start.col, e.code))
}

fn ron_options() -> ron::Options {
    ron::Options::default().with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
}

fn ron_pretty() -> ron::ser::PrettyConfig {
    ron::ser::PrettyConfig::new()
        // The type's name is this program's business, not the file's.
        .struct_names(false)
        .indentor("    ")
        .extensions(ron::extensions::Extensions::IMPLICIT_SOME)
}

/// One rung's document and the file that answered — the `.ron`, or,
/// where none stands, the `Key=Value` file that came before it.
///
/// `Ok(None)` when the rung says nothing at all — no file THERE, which
/// is the ordinary case. `Err` when a file EXISTS and could not be
/// turned into a document, whether the obstacle was the syntax, the
/// permissions or the denylist; reading that as silence would invite a
/// writer to take the rung for empty and replace somebody's settings.
pub fn read_level(
    guard: &Denylist,
    level: &ConfLevel,
) -> Result<Option<(DesktopConf, PathBuf)>, ToolError> {
    if level.ron.symlink_metadata().is_ok() {
        let text = guard.read_to_string(&level.ron)?;
        return match parse_ron(&text) {
            Ok(doc) => Ok(Some((doc, level.ron.clone()))),
            Err(said) => Err(ToolError::Rejected {
                reason: format!("{} could not be read at {said}", level.ron.display()),
            }),
        };
    }
    if level.legacy.symlink_metadata().is_ok() {
        let text = guard.read_to_string(&level.legacy)?;
        return Ok(Some((
            DesktopConf::from_legacy(&legacy_map(&text)),
            level.legacy.clone(),
        )));
    }
    Ok(None)
}

/// The value the cascade of `levels` (most specific FIRST) gives each
/// key this agent knows, with the file it came from, sorted by key.
pub fn effective(guard: &Denylist, levels: &[ConfLevel]) -> Effective {
    let mut docs: Vec<(DesktopConf, PathBuf)> = Vec::new();
    let mut problems = Vec::new();
    for level in levels {
        match read_level(guard, level) {
            Ok(Some(found)) => docs.push(found),
            Ok(None) => {}
            Err(e) => problems.push(format!(
                "{e} — it is being ignored whole, so every setting in it counts as unset"
            )),
        }
    }
    let mut settings = Vec::new();
    for key in KEYS {
        // Most specific first: the first rung that answers, wins.
        for (doc, from) in &docs {
            if let Some(value) = field_value(doc, key.name) {
                settings.push(Setting {
                    key: key.name.to_string(),
                    value,
                    from: from.clone(),
                });
                break;
            }
        }
    }
    settings.sort_by(|a, b| a.key.cmp(&b.key));
    Effective { settings, problems }
}

// ------------------------------------------------------------- writing

/// What a change to the user's file did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub key: &'static str,
    pub value: String,
    /// What the user's own files said before — `None` when they said
    /// nothing and the system end of the cascade was answering.
    pub previous: Option<String>,
    pub written: Replaced,
}

/// The line every document this program writes opens with.
const CONF_HEADER: &str = "\
// nacelle-desktop.ron — the nacelle desktop's configuration.
// This file was last written by nacelle-ai, on the user's request.
";

/// Set one key in the user's own `nacelle/nacelle-desktop.ron`.
///
/// The value is checked first, and the document to write is SEEDED from
/// the user's own files — the `.ron` if one stands, else the `Key=Value`
/// file that came before it, the old folder one rung behind the new —
/// so the first write in the new format carries everything the user had
/// set and loses nothing. The system end of the cascade is deliberately
/// not seeded from: a value that came from `/etc/xdg` must stay a
/// system value.
///
/// A user file that exists and cannot be read is a REFUSAL, not an
/// empty seed: writing over a document whose contents this program
/// could not see is how settings get lost with nothing said.
pub fn set(
    dirs: &DesktopDirs,
    guard: &Denylist,
    key: &ConfKey,
    value: &str,
) -> Result<Change, ToolError> {
    let value = key.check(value)?;
    let dir = dirs.config_dir()?;
    std::fs::create_dir_all(dir).map_err(|e| ToolError::io(dir, &e))?;
    // Confinement even though this path was built here and not by a
    // model: it is the write that has to be inside the configuration
    // directory, and a check that only runs on some paths is a check
    // that will one day be forgotten on the one that mattered.
    let path = confine(dir, &dirs.user_conf()?)?;

    // Least specific first, so the more specific file wins field by
    // field — the same fold the read does, over the user's rungs only.
    let mut doc = DesktopConf::default();
    for level in dirs.user_conf_levels().iter().rev() {
        if let Some((found, _)) = read_level(guard, level).map_err(refuse_to_write_over)? {
            doc = found.over(doc);
        }
    }
    let previous = field_value(&doc, key.name);
    apply(&mut doc, key, &value);

    let body = ron_options()
        .to_string_pretty(&doc, ron_pretty())
        .map_err(|e| ToolError::Rejected {
            reason: format!("the new document could not be rendered: {e}; nothing was written"),
        })?;
    let text = format!("{CONF_HEADER}{body}\n");
    verify(&text, key, &value)?;

    let written = write::replace(&path, &text)?;
    Ok(Change {
        key: key.name,
        value,
        previous,
        written,
    })
}

/// The sentence a write answers when a user file could not be read.
fn refuse_to_write_over(e: ToolError) -> ToolError {
    ToolError::Rejected {
        reason: format!("{e}; nothing was written — repair or move the file first"),
    }
}

/// Read the finished text back and confirm it says what was asked,
/// before the old file is replaced rather than after.
fn verify(text: &str, key: &ConfKey, value: &str) -> Result<(), ToolError> {
    let doc = parse_ron(text).map_err(|said| ToolError::Rejected {
        reason: format!(
            "the edited document does not parse back ({said}); the file was left untouched"
        ),
    })?;
    let read_back = field_value(&doc, key.name);
    let wanted = if value.is_empty() {
        // Clearing removes the field for most keys and writes an
        // explicit off for the two offable ones; either way the field
        // must no longer answer with a NAME.
        None
    } else {
        Some(value.to_string())
    };
    let matches = match (&read_back, &wanted) {
        (got, want) if got == want => true,
        // A cleared offable key reads back as the empty string.
        (Some(s), None) if s.is_empty() => true,
        _ => false,
    };
    if matches {
        Ok(())
    } else {
        Err(ToolError::Rejected {
            reason: format!(
                "the edited document would read {}={} instead of {}={}; \
                 the file was left untouched",
                key.name,
                read_back.unwrap_or_default(),
                key.name,
                value
            ),
        })
    }
}
