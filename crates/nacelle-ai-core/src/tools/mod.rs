//! The agent's hands on the nacelle environment.
//!
//! There is no channel to a RUNNING nacelle-desktop yet — that is a
//! later stage of the project — so every tool here works on the FILES
//! the desktop reads: its `nacelle-desktop.ron` (and the `Key=Value`
//! file that came before it, read-only), the layauts, the themes and
//! the addons on the XDG search path. A change therefore
//! takes effect when the desktop next reads the file, not when the tool
//! returns, and every tool that writes says exactly that in its own
//! description. A model that promised the user an instant effect would
//! be promising something this program cannot deliver.
//!
//! Everything that writes obeys four rules, in this order:
//!
//! 1. **Confinement.** The path is resolved canonically — `..`
//!    collapsed, symlinks followed — and must land inside the user's
//!    configuration directory. Anything else is a refusal, not a write.
//!    See [`paths::confine`].
//! 2. **Validation.** The value is checked, the whole new text is built
//!    and read back, and only then is anything on disk touched. A
//!    rejected value never reaches the filesystem.
//! 3. **Backup.** The previous contents are copied beside the file as
//!    `<name>.bak`.
//! 4. **Atomic replace.** The new text is written to a temporary file
//!    in the same directory and renamed over the target, so an
//!    interrupted write leaves the old file or the new one and never
//!    half of either. See [`write::replace`].
//!
//! Results are JSON text. Every provider's tool-result field carries a
//! string, JSON is what a model reads back most reliably, and it is
//! what the tool INPUTS already are. Failures are results too:
//! [`Toolbox::run`] returns a [`ToolError`] whose message is written for
//! the model, and [`Toolbox::result_for`] turns one into a tool-result
//! block with `is_error` set — which is how the model finds out to try
//! something else instead of telling the user it worked.
//!
//! Every READ goes through the denylist in
//! [`redact::deny`](crate::redact::deny) before it happens. Not because
//! a theme directory is likely to hold an SSH key, but because a file in
//! it can be a symlink to one, and because a rule that is applied on
//! some read paths and not others is a rule that will be missing from
//! the one that mattered. The guard is a parameter to every function
//! here that opens a file, so a read that forgot it does not compile.
//!
//! Nothing here reads the process environment on its own. A [`Toolbox`]
//! is built from an [`Env`], so the desktop passes the real one and a
//! test passes a map pointing at a throwaway directory.
//!
//! [`Toolbox`] is also the agent loop's
//! [`ToolRegistry`](crate::agent::ToolRegistry) — including which calls
//! have to be put to the user before they run. That implementation
//! lives in a submodule here rather than beside the trait, because the
//! loop is written to drive any set of tools and should not have to
//! know that a desktop exists.

pub mod catalog;
pub mod conf;
pub mod error;
pub mod model;
pub mod paths;
mod registry;
pub mod write;

use serde_json::{json, Map, Value};

use crate::credentials::Env;
use crate::message::{Content, ToolCall, ToolDeclaration};
use crate::redact::deny::Denylist;
use crate::tools::catalog::{AddonEntry, BUILTIN_LAYAUT};
use crate::tools::conf::{Change, ConfKey, Setting, KEYS};
use crate::tools::error::ToolError;
use crate::tools::paths::DesktopDirs;

pub const TOOL_LIST_THEMES: &str = "nacelle_list_themes";
pub const TOOL_SET_THEME: &str = "nacelle_set_theme";
pub const TOOL_LIST_LAYAUTS: &str = "nacelle_list_layauts";
pub const TOOL_READ_LAYAUT: &str = "nacelle_read_layaut";
pub const TOOL_SET_LAYAUT: &str = "nacelle_set_layaut";
pub const TOOL_LIST_ADDONS: &str = "nacelle_list_addons";
pub const TOOL_READ_CONFIG: &str = "nacelle_read_config";
pub const TOOL_SET_CONFIG: &str = "nacelle_set_config";

/// The sentence every writing tool's description ends with, and the
/// reason this module exists in the shape it does: files are the only
/// channel to the desktop today.
const TAKES_EFFECT: &str = "This edits a file on disk. It does NOT change a desktop that is \
     already running: nacelle-desktop reads its configuration when it starts, and nothing \
     watches the file for changes. Tell the user the change is saved and will apply the next \
     time the desktop starts — never that the desktop has already changed.";

/// The same fact, short enough to repeat in every result.
const TAKES_EFFECT_SHORT: &str =
    "saved to the configuration file; nacelle-desktop applies it the next time it starts";

/// What a listing of installed FILES cannot see.
const BUILTIN_THEMES_NOTE: &str =
    "Only installed .theme files are listed. The toolkit also carries themes compiled into it, \
     which are not files and cannot be listed here — an active theme that is missing from this \
     list is most likely one of those.";

const BUILTIN_WIDGETS_NOTE: &str =
    "Only installed addon files are listed. nacelle-desktop also links some widgets straight \
     into its binary; those are not files and cannot be listed here.";

/// The tools, over one installation's directories.
///
/// Stateless apart from where the desktop keeps its files: each call
/// reads what is on disk at that moment, because between two calls the
/// user may have changed something by hand and a cached answer would be
/// a confident lie.
#[derive(Clone, Debug)]
pub struct Toolbox {
    dirs: DesktopDirs,
    guard: Denylist,
}

impl Toolbox {
    /// A toolbox with the name, extension and content rules of the
    /// denylist but none of its home-relative directories — see
    /// [`Denylist::new`]. [`Toolbox::from_env`] is what a real
    /// installation uses; this is for an embedder that has already
    /// decided where everything lives.
    pub fn new(dirs: DesktopDirs) -> Self {
        Toolbox {
            dirs,
            guard: Denylist::new(None),
        }
    }

    /// The toolbox for the installation the given environment
    /// describes.
    pub fn from_env(env: &dyn Env) -> Self {
        Toolbox {
            dirs: DesktopDirs::from_env(env),
            guard: Denylist::from_env(env),
        }
    }

    /// Use a denylist built elsewhere — one the embedder has added its
    /// own directories to, say.
    ///
    /// This cannot weaken anything. The name, extension and content
    /// rules are part of every [`Denylist`] there is and cannot be
    /// constructed without, so the poorest list that can be passed here
    /// is the one [`Toolbox::new`] would have made.
    pub fn with_guard(mut self, guard: Denylist) -> Self {
        self.guard = guard;
        self
    }

    pub fn dirs(&self) -> &DesktopDirs {
        &self.dirs
    }

    /// The denylist every read in this toolbox goes through.
    pub fn guard(&self) -> &Denylist {
        &self.guard
    }

    /// What the model is told it may call.
    ///
    /// The descriptions are written for a model deciding WHEN to reach
    /// for a tool, which is why each one says what it changes and what
    /// it cannot do, rather than restating its own name.
    pub fn declarations(&self) -> Vec<ToolDeclaration> {
        vec![
            ToolDeclaration::new(
                TOOL_LIST_THEMES,
                format!(
                    "List the themes installed for the nacelle desktop and say which one the \
                     configuration currently selects. Read-only. {BUILTIN_THEMES_NOTE}"
                ),
                no_arguments(),
            ),
            ToolDeclaration::new(
                TOOL_SET_THEME,
                format!(
                    "Choose the theme the nacelle desktop draws itself with, by setting the \
                     theme in the user's nacelle-desktop.ron. Call {TOOL_LIST_THEMES} first if \
                     the user named a theme you have not seen. {TAKES_EFFECT}"
                ),
                one_string(
                    "name",
                    "Theme name without the .theme extension, e.g. \"crimson\". \
                     Letters, digits, '_' and '-' only. An empty string clears the setting \
                     and leaves the toolkit's built-in theme.",
                ),
            ),
            ToolDeclaration::new(
                TOOL_LIST_LAYAUTS,
                "List the layauts (panel layouts) installed for the nacelle desktop and say \
                 which one the configuration currently selects. Read-only."
                    .to_string(),
                no_arguments(),
            ),
            ToolDeclaration::new(
                TOOL_READ_LAYAUT,
                "Read one layaut file as text — the columns, the panels and their heights. \
                 Read-only. Use it to answer questions about how the desktop is arranged, or \
                 before suggesting a change to a layaut."
                    .to_string(),
                one_string(
                    "name",
                    "Layaut name without the .layaut extension, as listed by \
                     nacelle_list_layauts.",
                ),
            ),
            ToolDeclaration::new(
                TOOL_SET_LAYAUT,
                format!(
                    "Choose the layaut the nacelle desktop arranges its panels with, by setting \
                     the layaut in the user's nacelle-desktop.ron. Only a layaut that is \
                     installed may be chosen. {TAKES_EFFECT}"
                ),
                one_string(
                    "name",
                    "Layaut name without the .layaut extension, as listed by \
                     nacelle_list_layauts. An empty string clears the setting and leaves the \
                     built-in default layaut.",
                ),
            ),
            ToolDeclaration::new(
                TOOL_LIST_ADDONS,
                format!(
                    "List the addons installed for the nacelle desktop — the .rhai scripts and \
                     the compiled .so plugins that draw its panels — with whatever each one \
                     declares about itself. Read-only, and nothing is executed or loaded. \
                     {BUILTIN_WIDGETS_NOTE}"
                ),
                no_arguments(),
            ),
            ToolDeclaration::new(
                TOOL_READ_CONFIG,
                "Read the nacelle desktop's effective configuration: every setting in force, \
                 which file it came from, and the full list of keys that may be set with \
                 nacelle_set_config and what each one accepts. Read-only. Call this before \
                 changing a setting you are not certain of."
                    .to_string(),
                no_arguments(),
            ),
            ToolDeclaration::new(
                TOOL_SET_CONFIG,
                format!(
                    "Set one key in the user's nacelle-desktop.ron. Only the keys \
                     nacelle_read_config lists may be set, and only to a value of the shape it \
                     gives; anything else is refused and nothing is written. The theme and the \
                     layaut have tools of their own ({TOOL_SET_THEME}, {TOOL_SET_LAYAUT}) which \
                     also check that what is being chosen is installed — prefer those. \
                     {TAKES_EFFECT}"
                ),
                json!({
                    "type": "object",
                    "properties": {
                        "key": {
                            "type": "string",
                            "description": "Key to set, exactly as nacelle_read_config spells \
                                            it, e.g. \"SoundVolume\"."
                        },
                        "value": {
                            "type": ["string", "number", "boolean"],
                            "description": "The new value. An empty string clears the key: for \
                                            most keys the setting is removed so the system \
                                            defaults answer again; for ColorLut and ColorIcc it \
                                            is stored as an explicit \"none\" that overrides \
                                            any system-wide default."
                        }
                    },
                    "required": ["key", "value"],
                    "additionalProperties": false
                }),
            ),
        ]
    }

    /// Run one tool and return its result as JSON text.
    pub fn run(&self, name: &str, input: &Value) -> Result<String, ToolError> {
        let value = match name {
            TOOL_LIST_THEMES => self.list_themes(),
            TOOL_SET_THEME => self.set_theme(input),
            TOOL_LIST_LAYAUTS => self.list_layauts(),
            TOOL_READ_LAYAUT => self.read_layaut(input),
            TOOL_SET_LAYAUT => self.set_layaut(input),
            TOOL_LIST_ADDONS => self.list_addons(),
            TOOL_READ_CONFIG => self.read_config(),
            TOOL_SET_CONFIG => self.set_config(input),
            other => Err(ToolError::UnknownTool {
                name: other.to_string(),
            }),
        }?;
        // A Value built in this module cannot fail to serialise; the
        // compact rendering is the harmless answer if that ever stops
        // being true.
        Ok(serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()))
    }

    /// Run the call and package the answer as the conversation block a
    /// backend sends back.
    ///
    /// A failure is a result with `is_error` set, not a missing block:
    /// a model that asked for something impossible has to be told, and
    /// a turn with a tool call and no matching result is a broken
    /// conversation on every provider.
    pub fn result_for(&self, call: &ToolCall) -> Content {
        match self.run(&call.name, &call.input) {
            Ok(output) => Content::ToolResult {
                id: call.id.clone(),
                output,
                is_error: false,
            },
            Err(e) => Content::ToolResult {
                id: call.id.clone(),
                output: e.to_string(),
                is_error: true,
            },
        }
    }

    // ---- the tools themselves -------------------------------------

    fn list_themes(&self) -> Result<Value, ToolError> {
        let installed = catalog::themes(&self.dirs);
        let active = self.setting("Theme");
        Ok(json!({
            "active": active.as_ref().map(|s| s.value.clone()),
            "active_from": active.as_ref().map(|s| path(&s.from)),
            "installed": installed
                .iter()
                .map(|t| json!({ "name": t.name, "path": path(&t.path) }))
                .collect::<Vec<_>>(),
            "searched": self
                .dirs
                .theme_dirs()
                .iter()
                .map(|d| path(d))
                .collect::<Vec<_>>(),
            "note": BUILTIN_THEMES_NOTE,
        }))
    }

    fn set_theme(&self, input: &Value) -> Result<Value, ToolError> {
        let name = string_field(TOOL_SET_THEME, input, "name")?;
        let key = conf_key("Theme");
        let change = conf::set(&self.dirs, &self.guard, key, &name)?;
        let mut result = changed(&change);
        // Not an error: the toolkit carries themes compiled into it, so
        // a name that is not a file may still be perfectly good. It is
        // worth saying, because the other possibility is a typo that
        // will leave the desktop on its default.
        if !change.value.is_empty()
            && !catalog::themes(&self.dirs)
                .iter()
                .any(|t| t.name == change.value)
        {
            insert(
                &mut result,
                "warning",
                json!(format!(
                    "no {}.theme is installed; if it is not one of the themes compiled into \
                     the toolkit, nacelle-desktop will fall back to its built-in theme",
                    change.value
                )),
            );
        }
        Ok(result)
    }

    fn list_layauts(&self) -> Result<Value, ToolError> {
        let installed = catalog::layauts(&self.dirs);
        let active = self.setting("Layaut");
        Ok(json!({
            "active": active.as_ref().map(|s| s.value.clone()),
            "active_from": active.as_ref().map(|s| path(&s.from)),
            "installed": installed
                .iter()
                .map(|l| json!({
                    "name": l.name,
                    "path": l.path.as_deref().map(path),
                    "built_in": l.path.is_none(),
                }))
                .collect::<Vec<_>>(),
            "searched": self
                .dirs
                .data_roots()
                .iter()
                .map(|d| path(&d.join(paths::LAYAUTS_SUB)))
                .collect::<Vec<_>>(),
        }))
    }

    fn read_layaut(&self, input: &Value) -> Result<Value, ToolError> {
        let name = string_field(TOOL_READ_LAYAUT, input, "name")?;
        if let Some((found, text)) = catalog::read_layaut(&self.dirs, &self.guard, &name)? {
            return Ok(json!({
                "name": name.trim(),
                "path": path(&found),
                "text": text,
            }));
        }
        if name.trim() == BUILTIN_LAYAUT {
            return Ok(json!({
                "name": BUILTIN_LAYAUT,
                "path": Value::Null,
                "text": Value::Null,
                "note": "the built-in default layaut has no file — the toolkit computes it, \
                         and it can only be read by installing a layaut of the same name",
            }));
        }
        Err(ToolError::NotFound {
            what: format!("layaut \"{}\"", name.trim()),
        })
    }

    fn set_layaut(&self, input: &Value) -> Result<Value, ToolError> {
        let name = string_field(TOOL_SET_LAYAUT, input, "name")?;
        let key = conf_key("Layaut");
        // Checked BEFORE the write, unlike the theme: the only layaut
        // that exists without a file is the built-in default, so a name
        // that is not installed is a mistake and can be reported as one
        // while the file is still untouched.
        let wanted = key.check(&name)?;
        let installed = catalog::layauts(&self.dirs);
        if !wanted.is_empty() && !installed.iter().any(|l| l.name == wanted) {
            return Err(ToolError::NotFound {
                what: format!(
                    "layaut \"{wanted}\" — installed: {}",
                    installed
                        .iter()
                        .map(|l| l.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
        Ok(changed(&conf::set(&self.dirs, &self.guard, key, &wanted)?))
    }

    fn list_addons(&self) -> Result<Value, ToolError> {
        let addons = catalog::addons(&self.dirs, &self.guard);
        Ok(json!({
            "addons": addons.iter().map(addon).collect::<Vec<_>>(),
            "searched": self
                .dirs
                .data_roots()
                .iter()
                .map(|d| path(&d.join(paths::ADDONS_SUB)))
                .collect::<Vec<_>>(),
            "note": BUILTIN_WIDGETS_NOTE,
        }))
    }

    fn read_config(&self) -> Result<Value, ToolError> {
        let levels = self.dirs.conf_levels();
        let effective = conf::effective(&self.guard, &levels);
        let mut result = json!({
            "user_file": self.dirs.user_conf().ok().as_deref().map(path),
            "files": levels
                .iter()
                .flat_map(|l| {
                    [
                        json!({ "path": path(&l.ron), "format": "ron",
                                "exists": l.ron.is_file() }),
                        json!({ "path": path(&l.legacy), "format": "key=value (read-only)",
                                "exists": l.legacy.is_file() }),
                    ]
                })
                .collect::<Vec<_>>(),
            "settings": effective
                .settings
                .iter()
                .map(|s| json!({
                    "key": s.key,
                    "value": s.value,
                    "from": path(&s.from),
                }))
                .collect::<Vec<_>>(),
            "keys": KEYS
                .iter()
                .map(|k| json!({
                    "key": k.name,
                    "accepts": k.accepts.describe(),
                    "summary": k.summary,
                }))
                .collect::<Vec<_>>(),
            "note": "The configuration is RON (nacelle-desktop.ron), read as a cascade: the \
                     user's file first, then the system ones, setting by setting. A directory \
                     with no .ron is answered by its old Key=Value file, which is read but \
                     never written. An empty value means an explicit \"none\".",
        });
        if !effective.problems.is_empty() {
            insert(
                &mut result,
                "problems",
                json!(effective.problems),
            );
        }
        Ok(result)
    }

    fn set_config(&self, input: &Value) -> Result<Value, ToolError> {
        let name = string_field(TOOL_SET_CONFIG, input, "key")?;
        let name = name.trim();
        let value = scalar_field(TOOL_SET_CONFIG, input, "value")?;
        let Some(key) = conf::key(name) else {
            return Err(ToolError::Rejected {
                reason: format!(
                    "\"{name}\" is not a key nacelle-desktop reads. It knows: {}",
                    KEYS.iter().map(|k| k.name).collect::<Vec<_>>().join(", ")
                ),
            });
        };
        Ok(changed(&conf::set(&self.dirs, &self.guard, key, &value)?))
    }

    /// The effective value of one configuration key.
    fn setting(&self, key: &str) -> Option<Setting> {
        conf::effective(&self.guard, &self.dirs.conf_levels())
            .settings
            .into_iter()
            .find(|s| s.key == key)
    }
}

/// The result every writing tool returns.
fn changed(change: &Change) -> Value {
    json!({
        "key": change.key,
        "value": change.value,
        "previous": change.previous,
        "file": path(&change.written.path),
        "backup": change.written.backup.as_deref().map(path),
        "takes_effect": TAKES_EFFECT_SHORT,
    })
}

fn addon(entry: &AddonEntry) -> Value {
    json!({
        "name": entry.name,
        "kind": entry.kind.as_str(),
        "path": path(&entry.path),
        "label": entry.label,
        "category": entry.category,
        "ref_h": entry.ref_h,
        "min_h": entry.min_h,
    })
}

/// A key that is known to be in [`KEYS`] because it is written here.
/// The alternative is threading an `Option` out of a lookup that cannot
/// fail, which would only invite a caller to handle a case that does
/// not exist.
fn conf_key(name: &'static str) -> &'static ConfKey {
    conf::key(name).expect("a key named in this module is one of the KEYS")
}

fn path(p: &std::path::Path) -> String {
    p.display().to_string()
}

fn insert(target: &mut Value, key: &str, value: Value) {
    if let Some(map) = target.as_object_mut() {
        map.insert(key.to_string(), value);
    }
}

fn no_arguments() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

fn one_string(field: &str, description: &str) -> Value {
    json!({
        "type": "object",
        "properties": { field: { "type": "string", "description": description } },
        "required": [field],
        "additionalProperties": false
    })
}

/// The arguments as an object. A model that has nothing to pass may
/// send `null` or leave the field out entirely, and both mean the same
/// thing as `{}` — refusing them would be pedantry that costs a turn.
fn arguments(tool: &'static str, input: &Value) -> Result<Map<String, Value>, ToolError> {
    match input {
        Value::Null => Ok(Map::new()),
        Value::Object(map) => Ok(map.clone()),
        _ => Err(ToolError::BadInput {
            tool,
            reason: "the arguments must be a JSON object".to_string(),
        }),
    }
}

fn string_field(tool: &'static str, input: &Value, field: &str) -> Result<String, ToolError> {
    match arguments(tool, input)?.get(field) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(ToolError::BadInput {
            tool,
            reason: format!("\"{field}\" must be a string"),
        }),
        None => Err(ToolError::BadInput {
            tool,
            reason: format!("\"{field}\" is required"),
        }),
    }
}

/// A field that may arrive as a string, a number or a boolean.
///
/// The file this ends up in is text, so everything becomes text — but a
/// model that sends `50` for a volume or `true` for a switch is being
/// reasonable, and rejecting it would waste a turn teaching it a
/// distinction the format does not have.
fn scalar_field(tool: &'static str, input: &Value, field: &str) -> Result<String, ToolError> {
    match arguments(tool, input)?.get(field) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Number(n)) => Ok(n.to_string()),
        Some(Value::Bool(b)) => Ok(if *b { "1" } else { "0" }.to_string()),
        Some(_) => Err(ToolError::BadInput {
            tool,
            reason: format!("\"{field}\" must be a string, a number or true/false"),
        }),
        None => Err(ToolError::BadInput {
            tool,
            reason: format!("\"{field}\" is required"),
        }),
    }
}
