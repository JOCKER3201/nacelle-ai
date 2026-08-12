//! Where nacelle-desktop keeps its configuration and its data, and the
//! two rules that keep a tool inside those directories.
//!
//! The arrangement is the XDG one the desktop implements: exactly one
//! directory is ever WRITTEN to, and a search path — most specific
//! first — is READ, so a distribution can ship defaults that a user
//! install shadows without anything being copied into a home directory.
//!
//! ```text
//! $XDG_CONFIG_HOME/nacelle-desktop/            the user's own, and the only write target
//! $XDG_CONFIG_DIRS/nacelle-desktop/            system defaults (/etc/xdg when unset)
//! $XDG_DATA_HOME/nacelle-desktop/              layauts/, themes/, addons/, sounds/
//! $XDG_DATA_DIRS/nacelle-desktop/              the installed ones
//! ```
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

/// The desktop's name — its directory under every XDG root.
pub const APP: &str = "nacelle-desktop";
/// The one configuration file, in every configuration directory.
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
const SYSTEM_THEME_DIR: &str = "/usr/share/nacelle-desktop/themes";

/// Every directory a tool may read from, and the one it may write to.
#[derive(Clone, Debug)]
pub struct DesktopDirs {
    config_dir: Option<PathBuf>,
    config_dirs: Vec<PathBuf>,
    data_dirs: Vec<PathBuf>,
    theme_dirs: Vec<PathBuf>,
}

impl DesktopDirs {
    /// The directories the given environment describes.
    pub fn from_env(env: &dyn Env) -> Self {
        let config_dir = home_based(env, ENV_XDG_CONFIG_HOME, ".config");
        let data_dir = home_based(env, ENV_XDG_DATA_HOME, ".local/share");
        let config_dirs = search_path(
            config_dir.clone(),
            non_blank(env.var(ENV_XDG_CONFIG_DIRS)),
            DEFAULT_CONFIG_DIRS,
        );
        let data_dirs = search_path(
            data_dir.clone(),
            non_blank(env.var(ENV_XDG_DATA_DIRS)),
            DEFAULT_DATA_DIRS,
        );
        // The toolkit's own theme search path, which is NOT the data
        // search path: it honours NACELLE_THEME_DIR, looks in the
        // configuration directory as well as the data one, and ends at
        // a fixed system directory rather than walking XDG_DATA_DIRS.
        // Mirrored rather than invented, so this lists the files the
        // desktop would really load.
        let mut theme_dirs = Vec::new();
        if let Some(dir) = non_blank(env.var(ENV_THEME_DIR)) {
            theme_dirs.push(PathBuf::from(dir));
        }
        for dir in [data_dir, config_dir.clone()].into_iter().flatten() {
            push_unique(&mut theme_dirs, dir.join(THEMES_SUB));
        }
        push_unique(&mut theme_dirs, PathBuf::from(SYSTEM_THEME_DIR));
        DesktopDirs {
            config_dir,
            config_dirs,
            data_dirs,
            theme_dirs,
        }
    }

    /// Build directories directly — for tests, and for an embedder that
    /// keeps the desktop somewhere of its own.
    pub fn new(config_dir: Option<PathBuf>, data_dir: Option<PathBuf>) -> Self {
        let config_dirs = search_path(config_dir.clone(), None, "");
        let data_dirs = search_path(data_dir.clone(), None, "");
        let mut theme_dirs = Vec::new();
        for dir in [data_dir, config_dir.clone()].into_iter().flatten() {
            push_unique(&mut theme_dirs, dir.join(THEMES_SUB));
        }
        DesktopDirs {
            config_dir,
            config_dirs,
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

    /// Every `nacelle-desktop.conf` that takes part, most specific
    /// first. Files that do not exist are included: which of them is
    /// missing is part of the answer to "where does this value come
    /// from".
    pub fn conf_files(&self) -> Vec<PathBuf> {
        self.config_dirs.iter().map(|d| d.join(CONF_FILE)).collect()
    }

    /// The user's own `nacelle-desktop.conf` — the only one a tool may
    /// write. The system copies belong to the package manager.
    pub fn user_conf(&self) -> Result<PathBuf, ToolError> {
        Ok(self.config_dir()?.join(CONF_FILE))
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
}

/// `$VAR/nacelle-desktop`, or `$HOME/<fallback>/nacelle-desktop`, or
/// nothing at all when the environment names neither.
fn home_based(env: &dyn Env, var: &str, fallback: &str) -> Option<PathBuf> {
    let root = match non_blank(env.var(var)) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let mut home = PathBuf::from(non_blank(env.var(ENV_HOME))?);
            for part in fallback.split('/') {
                home.push(part);
            }
            home
        }
    };
    Some(root.join(APP))
}

/// The user's directory first, then each `:`-separated system base
/// joined with the application name, duplicates dropped.
fn search_path(user: Option<PathBuf>, system: Option<String>, default: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = user.into_iter().collect();
    let system = system.unwrap_or_else(|| default.to_string());
    for base in system.split(':').filter(|b| !b.is_empty()) {
        push_unique(&mut out, PathBuf::from(base).join(APP));
    }
    out
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
