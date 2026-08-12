//! `nacelle-desktop.conf`: how it is read, which keys it may carry, and
//! how exactly one of them is changed.
//!
//! The format is `Key=Value`, one per line, `#` and `;` comments, and
//! it is read as a CASCADE: the user's file laid over the system ones,
//! key by key, most specific first. A key the user never set is
//! answered by a system file, which is how a distribution ships a
//! default without copying anything into a home directory. Writes go to
//! the user's own file and nowhere else — the system copies belong to
//! whoever installed them.
//!
//! An empty value is a value, not an absence. `ColorLut=` in the user's
//! file means "no LUT" and beats a system file that names one, so
//! clearing a key is a real operation here rather than a request to
//! delete the line.
//!
//! [`KEYS`] is a closed list on purpose. A tool that would write any
//! key at all is a tool that lets a model quietly fill a user's
//! configuration with words the desktop has never heard of, and no
//! error would ever be raised — the desktop ignores what it does not
//! know. Refusing an unknown key and naming the ones that exist is the
//! only version of this the model can learn from. The cost is real and
//! worth stating: when the desktop grows a key, this list has to grow
//! with it, or the tool will refuse something legitimate. The ranges
//! below are the desktop's own — where it clamps a value, the range
//! here is that clamp, so nothing accepted is silently changed and
//! nothing refused would have had an effect.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::tools::error::ToolError;
use crate::tools::paths::{confine, is_identifier, safe_component, DesktopDirs};
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

/// One key the desktop reads.
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

/// Every key `nacelle-desktop.conf` may carry, in the desktop's own
/// grouping: appearance, fonts, sound, colour, glass, grid.
pub const KEYS: &[ConfKey] = &[
    ConfKey {
        name: "Theme",
        accepts: Accepts::Identifier,
        summary: "the theme the desktop loads; empty means the toolkit's built-in master theme",
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
        summary: "file name of a grading LUT installed under the data directory's lut/",
    },
    ConfKey {
        name: "ColorIcc",
        accepts: Accepts::Name,
        summary: "file name of an ICC profile installed under the data directory's icc/",
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
    /// accepted for every key: it is how the desktop is told "no value
    /// here", and it outranks a system default rather than falling back
    /// to one.
    pub fn check(&self, value: &str) -> Result<String, ToolError> {
        let value = value.trim();
        // A newline in a value would end the line early and turn the
        // rest into another Key=Value — one tool call writing two
        // settings, one of which nobody validated. This is the reason
        // this check exists at all, so it comes before every other one
        // and applies to every key.
        if value.contains('\n') || value.contains('\r') || value.contains('\0') {
            return Err(ToolError::Rejected {
                reason: format!(
                    "the value for {} contains a line break or a NUL, which would write \
                     a second setting nobody checked",
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

/// One effective setting, and the file it won in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setting {
    pub key: String,
    pub value: String,
    pub from: PathBuf,
}

/// Every `Key=Value` in the text, in file order, keys and values
/// trimmed. Comments and blank lines contribute nothing. Order is kept
/// rather than collapsed into a map because a file that sets the same
/// key twice is answered by its LAST line, and a caller has to be able
/// to see that.
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

/// The value the cascade of `files` (most specific FIRST) gives each
/// key, with the file it came from, sorted by key.
pub fn effective(files: &[PathBuf]) -> Vec<Setting> {
    let mut merged: BTreeMap<String, Setting> = BTreeMap::new();
    // Least specific first, so a more specific file overwrites what a
    // less specific one said.
    for path in files.iter().rev() {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for (key, value) in parse(&text) {
            merged.insert(
                key.clone(),
                Setting {
                    key,
                    value,
                    from: path.clone(),
                },
            );
        }
    }
    merged.into_values().collect()
}

/// What a change to the user's file did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub key: &'static str,
    pub value: String,
    /// What the cascade said before — `None` when nothing set it.
    pub previous: Option<String>,
    pub written: Replaced,
}

/// Set one key in the user's own `nacelle-desktop.conf`.
///
/// The value is checked, then the whole new text is built and re-read to
/// confirm it says what was asked, and only then does anything reach the
/// filesystem. The file is created — with its directory — on the first
/// write and not before: like the desktop, this program installs nothing
/// into a home directory until the user changes something.
pub fn set(dirs: &DesktopDirs, key: &ConfKey, value: &str) -> Result<Change, ToolError> {
    let value = key.check(value)?;
    let dir = dirs.config_dir()?;
    std::fs::create_dir_all(dir).map_err(|e| ToolError::io(dir, &e))?;
    // Confinement even though this path was built here and not by a
    // model: it is the write that has to be inside the configuration
    // directory, and a check that only runs on some paths is a check
    // that will one day be forgotten on the one that mattered. This
    // also catches a configuration directory that is a symlink out of
    // the user's tree.
    let path = confine(dir, &dirs.user_conf()?)?;

    let old = read_text(&path)?;
    let new = upsert(&old, key.name, &value);
    verify(&new, key.name, &value)?;
    let previous = last_value(&old, key.name);

    let written = write::replace(&path, &new)?;
    Ok(Change {
        key: key.name,
        value,
        previous,
        written,
    })
}

fn read_text(path: &Path) -> Result<String, ToolError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(ToolError::io(path, &e)),
    }
}

/// The value the file's own last declaration of `key` gives.
fn last_value(text: &str, key: &str) -> Option<String> {
    parse(text)
        .into_iter()
        .rfind(|(k, _)| k == key)
        .map(|(_, v)| v)
}

/// The text with `key` set to `value`, everything else — comments,
/// spacing, order — left alone.
///
/// EVERY line declaring the key is rewritten, not just the first. A
/// file that declared it twice would otherwise be left disagreeing with
/// itself, and since the desktop's reader takes the last declaration,
/// rewriting only the first would produce a file that says one thing
/// and means another.
fn upsert(text: &str, key: &str, value: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let mut found = false;
    for line in lines.iter_mut() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if let Some((declared, _)) = trimmed.split_once('=') {
            if declared.trim() == key {
                *line = format!("{key}={value}");
                found = true;
            }
        }
    }
    if !found {
        lines.push(format!("{key}={value}"));
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Read the finished text back and confirm it says what was asked.
///
/// The last line to mention the key wins, exactly as in the desktop's
/// reader, so this catches anything [`upsert`] could have got wrong
/// before the old file is replaced rather than after.
fn verify(text: &str, key: &str, value: &str) -> Result<(), ToolError> {
    match last_value(text, key) {
        Some(v) if v == value => Ok(()),
        other => Err(ToolError::Rejected {
            reason: format!(
                "the edited file would read {key}={} instead of {key}={value}; \
                 the file was left untouched",
                other.unwrap_or_default()
            ),
        }),
    }
}
