//! Where the nacelle family keeps its configuration and its data, and
//! the two rules that keep a tool inside those directories.
//!
//! The arrangement is the XDG one the desktop implements: exactly one
//! directory is ever WRITTEN to, and a search path — most specific
//! first — is READ, so a distribution can ship defaults that a user
//! install shadows without anything being copied into a home directory.
//!
//! ```text
//! $XDG_CONFIG_HOME/nacelle/            the user's own, and the only write target
//! $XDG_CONFIG_DIRS/nacelle/            system defaults (/etc/xdg when unset)
//! $XDG_DATA_HOME/nacelle/              layauts/, themes/, addons/, sounds/
//! $XDG_DATA_DIRS/nacelle/              the installed ones
//! ```
//!
//! Every one of those is followed on the read path by the same
//! directory under the folder's old name, `nacelle-desktop` — see
//! [`LEGACY_APP`]. This program is the reason the folder is named after
//! the family at all: it reads the desktop's directories, and a folder
//! named after one member of a family that shares it was an accident
//! rather than a design.
//!
//! The directories are derived from an [`Env`] rather than from the
//! process environment, so a test hands this a map and a throwaway
//! directory and no process-wide state takes part. That is also why
//! nothing here falls back to the current directory when `HOME` is
//! unset: a tool with nowhere legitimate to write must say so.
//!
//! [`confine`] is the other half. Everything a tool path is built from
//! that came from the MODEL goes through it, and it answers one
//! question: after resolving `..`, symlinks and all, is this still
//! inside the directory the tool is allowed in?

use std::path::{Component, Path, PathBuf};

use crate::credentials::Env;
use crate::tools::error::ToolError;

/// The family's name — its directory under every XDG root. The FOLDER
/// is the family; the file inside it is the program.
pub const APP: &str = "nacelle";
/// What that directory was called when it was named after the desktop
/// alone. Read, never written: a machine that has one keeps it, and
/// everything installed there goes on being found one place further
/// down the search path.
pub const LEGACY_APP: &str = "nacelle-desktop";
/// The configuration document, in every configuration directory. Named
/// after the PROGRAM whose settings it carries, which is why it does
/// not change with the folder. RON, per the owner's decision of
/// 2026-08-12 — see `.gap-program/decyzja-konfiguracja-ron.md`.
pub const CONF_RON: &str = "nacelle-desktop.ron";
/// The same configuration in the format that came before it. Read
/// where no `.ron` stands beside it, never written: a machine that had
/// settings before the change keeps exactly the file it had.
pub const CONF_FILE: &str = "nacelle-desktop.conf";

/// Layout files, `<name>.layaut`.
pub const LAYAUTS_SUB: &str = "layauts";
/// Theme files, `<name>.theme`.
pub const THEMES_SUB: &str = "themes";
/// Addons: `addons/scripts/<name>.rhai` and `addons/plugins/<name>.so`.
pub const ADDONS_SUB: &str = "addons";

pub const ENV_XDG_CONFIG_HOME: &str = "XDG_CONFIG_HOME";
pub const ENV_XDG_CONFIG_DIRS: &str = "XDG_CONFIG_DIRS";
pub const ENV_XDG_DATA_HOME: &str = "XDG_DATA_HOME";
pub const ENV_XDG_DATA_DIRS: &str = "XDG_DATA_DIRS";
pub const ENV_HOME: &str = "HOME";
/// The toolkit reads this one first when it looks for theme files, so a
/// listing that ignored it would not be a listing of what the desktop
/// would actually load.
pub const ENV_THEME_DIR: &str = "NACELLE_THEME_DIR";

/// The XDG defaults, spelled out so the fallbacks are visible.
const DEFAULT_CONFIG_DIRS: &str = "/etc/xdg";
const DEFAULT_DATA_DIRS: &str = "/usr/local/share:/usr/share";
const SYSTEM_THEME_BASE: &str = "/usr/share";

/// One rung of the configuration cascade: a directory, the RON
/// document in it, and the `Key=Value` file that came before it. The
/// legacy file is consulted only where no `.ron` stands beside it —
/// per DIRECTORY, so the two formats can stand on different rungs of
/// the same cascade.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfLevel {
    pub dir: PathBuf,
    pub ron: PathBuf,
    pub legacy: PathBuf,
}

/// Every directory a tool may read from, and the one it may write to.
#[derive(Clone, Debug)]
pub struct DesktopDirs {
    config_dir: Option<PathBuf>,
    config_dirs: Vec<PathBuf>,
    /// How many of the leading entries of `config_dirs` are the USER's
    /// own — the ones a write may seed its document from. A value that
    /// came from `/etc/xdg` must stay a system value, or the first
    /// setting anybody changed would freeze that day's defaults into
    /// their home directory forever.
    user_levels: usize,
    data_dirs: Vec<PathBuf>,
    theme_dirs: Vec<PathBuf>,
}

impl DesktopDirs {
    /// The directories the given environment describes.
    pub fn from_env(env: &dyn Env) -> Self {
        let config_home = home_based(env, ENV_XDG_CONFIG_HOME, ".config");
        let data_home = home_based(env, ENV_XDG_DATA_HOME, ".local/share");
        // The write target is the family directory and nothing else:
        // the old name is read, never written to.
        let config_dir = config_home.as_ref().map(|base| base.join(APP));
        let config_dirs = search_path(
            config_home.clone(),
            non_blank(env.var(ENV_XDG_CONFIG_DIRS)),
            DEFAULT_CONFIG_DIRS,
        );
        // The user's base contributes the leading pair — the family
        // folder and the folder's old name — and nothing else does.
        let user_levels = if config_home.is_some() { 2 } else { 0 };
        let data_dirs = search_path(
            data_home.clone(),
            non_blank(env.var(ENV_XDG_DATA_DIRS)),
            DEFAULT_DATA_DIRS,
        );
        // The toolkit's own theme search path, which is NOT the data
        // search path: it honours NACELLE_THEME_DIR, looks in the
        // configuration directory as well as the data one, and ends at
        // a fixed system directory rather than walking XDG_DATA_DIRS.
        // Mirrored rather than invented, so this lists the files the
        // desktop would really load.
        //
        // Both folder names are listed, newest first, for the same
        // reason they are on the other two paths. Note that the toolkit
        // itself still names ONLY the old folder in its theme lookup —
        // that is a fourth repository and a change of its own — so
        // today a `.theme` under the new name is listed here before the
        // toolkit has learned to load it.
        let mut theme_dirs = Vec::new();
        if let Some(dir) = non_blank(env.var(ENV_THEME_DIR)) {
            theme_dirs.push(PathBuf::from(dir));
        }
        for base in [data_home, config_home].into_iter().flatten() {
            for name in [APP, LEGACY_APP] {
                push_unique(&mut theme_dirs, base.join(name).join(THEMES_SUB));
            }
        }
        for name in [APP, LEGACY_APP] {
            push_unique(
                &mut theme_dirs,
                PathBuf::from(SYSTEM_THEME_BASE).join(name).join(THEMES_SUB),
            );
        }
        DesktopDirs {
            config_dir,
            config_dirs,
            user_levels,
            data_dirs,
            theme_dirs,
        }
    }

    /// Build directories directly — for tests, and for an embedder that
    /// keeps the desktop somewhere of its own.
    ///
    /// These are the directories THEMSELVES: no family name is joined
    /// on and no old name is paired with them, because a caller that
    /// names its own directory has already said everything there is to
    /// say about where it keeps things.
    pub fn new(config_dir: Option<PathBuf>, data_dir: Option<PathBuf>) -> Self {
        let config_dirs: Vec<PathBuf> = config_dir.clone().into_iter().collect();
        let user_levels = config_dirs.len();
        let data_dirs: Vec<PathBuf> = data_dir.clone().into_iter().collect();
        let mut theme_dirs = Vec::new();
        for dir in [data_dir, config_dir.clone()].into_iter().flatten() {
            push_unique(&mut theme_dirs, dir.join(THEMES_SUB));
        }
        DesktopDirs {
            config_dir,
            config_dirs,
            user_levels,
            data_dirs,
            theme_dirs,
        }
    }

    /// The one directory anything is ever written to.
    pub fn config_dir(&self) -> Result<&Path, ToolError> {
        self.config_dir
            .as_deref()
            .ok_or(ToolError::NoConfigDir)
    }

    /// Every rung of the configuration cascade, most specific first.
    /// Files that do not exist are included: which of them is missing
    /// is part of the answer to "where does this value come from".
    pub fn conf_levels(&self) -> Vec<ConfLevel> {
        self.config_dirs
            .iter()
            .map(|d| ConfLevel {
                dir: d.clone(),
                ron: d.join(CONF_RON),
                legacy: d.join(CONF_FILE),
            })
            .collect()
    }

    /// The leading rungs of [`DesktopDirs::conf_levels`] that are the
    /// user's own — the ones a write may seed its document from.
    pub fn user_conf_levels(&self) -> Vec<ConfLevel> {
        let mut levels = self.conf_levels();
        levels.truncate(self.user_levels);
        levels
    }

    /// The user's own `nacelle-desktop.ron` — the only file a tool may
    /// write. The system copies belong to the package manager, and the
    /// old `Key=Value` file is read, never written.
    pub fn user_conf(&self) -> Result<PathBuf, ToolError> {
        Ok(self.config_dir()?.join(CONF_RON))
    }

    /// Sub-directories named `sub` that exist, in search order.
    pub fn asset_dirs(&self, sub: &str) -> Vec<PathBuf> {
        self.data_dirs
            .iter()
            .map(|d| d.join(sub))
            .filter(|d| d.is_dir())
            .collect()
    }

    /// The toolkit's theme directories that exist, in search order.
    pub fn theme_dirs(&self) -> Vec<PathBuf> {
        self.theme_dirs.iter().filter(|d| d.is_dir()).cloned().collect()
    }

    /// Every data root, whether or not it exists — what a listing
    /// reports as "looked here", so an empty result can be explained.
    pub fn data_roots(&self) -> &[PathBuf] {
        &self.data_dirs
    }

    /// The directories under the folder's OLD name that actually exist
    /// — the ones a read can still land in.
    ///
    /// Empty on a machine that never ran the desktop before the rename,
    /// which is why it is worth saying when it is not: this crate has
    /// no logging of its own, so it hands the fact to whoever runs it
    /// rather than printing from a library.
    pub fn legacy_dirs_in_use(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for dir in self.config_dirs.iter().chain(self.data_dirs.iter()) {
            if dir.file_name().and_then(|n| n.to_str()) == Some(LEGACY_APP) && dir.is_dir() {
                push_unique(&mut out, dir.clone());
            }
        }
        out
    }
}

/// `$VAR`, or `$HOME/<fallback>`, or nothing at all when the
/// environment names neither. The BASE the family directory sits in —
/// the search path needs it to build both names.
fn home_based(env: &dyn Env, var: &str, fallback: &str) -> Option<PathBuf> {
    match non_blank(env.var(var)) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => {
            let mut home = PathBuf::from(non_blank(env.var(ENV_HOME))?);
            for part in fallback.split('/') {
                home.push(part);
            }
            Some(home)
        }
    }
}

/// The user's base first, then each `:`-separated system base, every
/// one of them contributing `<base>/nacelle` and directly behind it
/// `<base>/nacelle-desktop`, duplicates dropped.
///
/// The pair is kept together at every level rather than the old names
/// being appended at the end: the configuration is merged key by key,
/// so a user's file under the old name has to go on outranking the
/// system defaults.
fn search_path(user: Option<PathBuf>, system: Option<String>, default: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for base in user.into_iter() {
        push_level(&mut out, &base);
    }
    let system = system.unwrap_or_else(|| default.to_string());
    for base in system.split(':').filter(|b| !b.is_empty()) {
        push_level(&mut out, Path::new(base));
    }
    out
}

/// One level of a search path: the family directory, then the folder's
/// old name.
fn push_level(out: &mut Vec<PathBuf>, base: &Path) {
    for name in [APP, LEGACY_APP] {
        push_unique(out, base.join(name));
    }
}

fn push_unique(list: &mut Vec<PathBuf>, dir: PathBuf) {
    if !list.contains(&dir) {
        list.push(dir);
    }
}

fn non_blank(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// One safe path component: not empty, not `.`, not `..`, no separator,
/// no NUL — the rule that stops a name from being a path. The same rule
/// the desktop applies to `Layaut=` and `Sounds=`, because a name that
/// reaches this program from a model is exactly as untrusted as one
/// that reaches the desktop from a configuration file.
pub fn safe_component(name: &str) -> Option<String> {
    let n = name.trim();
    if n.is_empty() || n == "." || n == ".." {
        return None;
    }
    if n.contains('/') || n.contains('\\') || n.contains('\0') {
        return None;
    }
    let mut comps = Path::new(n).components();
    match (comps.next(), comps.next()) {
        (Some(Component::Normal(c)), None) if c == n => Some(n.to_string()),
        _ => None,
    }
}

/// A bare identifier: letters, digits, `_` and `-`. What a theme name
/// is allowed to be, because the toolkit resolves `[meta] base` by name
/// and a name that could be a path would be a file-read primitive.
pub fn is_identifier(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `candidate`, resolved canonically, if and only if it lands inside
/// `root`.
///
/// Canonical resolution is the whole point: `..` is collapsed and every
/// symlink is followed, so a link inside the directory that points at
/// `/etc` is caught by the same check that catches `../../etc`. Both
/// sides are resolved, because the root itself is very often reached
/// through a link — `/home` is a symlink to `/var/home` on this
/// project's own machine, and a comparison of unresolved strings would
/// call every legitimate path an escape.
///
/// A file that does not exist yet still has to be placeable: its
/// PARENT is resolved instead and the final component, which must be a
/// plain name, is joined back on. That is the case every write takes,
/// since the first write to a fresh installation creates the file.
pub fn confine(root: &Path, candidate: &Path) -> Result<PathBuf, ToolError> {
    let shown = candidate.display().to_string();
    let root = root
        .canonicalize()
        .map_err(|e| ToolError::io(root, &e))?;
    let inside = |p: &Path| p.starts_with(&root);

    if candidate.symlink_metadata().is_ok() {
        let real = candidate
            .canonicalize()
            .map_err(|e| ToolError::io(candidate, &e))?;
        return if inside(&real) {
            Ok(real)
        } else {
            Err(ToolError::Outside { input: shown, root })
        };
    }

    let (Some(parent), Some(name)) = (candidate.parent(), candidate.file_name()) else {
        return Err(ToolError::Outside { input: shown, root });
    };
    // A final component that is not a plain name — `a/..`, or a bare
    // `/` — cannot be joined back on safely, so it is refused rather
    // than normalised into something the caller did not ask for.
    if safe_component(&name.to_string_lossy()).is_none() {
        return Err(ToolError::Outside { input: shown, root });
    }
    let parent = parent
        .canonicalize()
        .map_err(|e| ToolError::io(parent, &e))?;
    if !inside(&parent) {
        return Err(ToolError::Outside { input: shown, root });
    }
    Ok(parent.join(name))
}
