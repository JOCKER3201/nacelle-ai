//! What is installed: themes, layauts, addons.
//!
//! All of it is a directory scan, and all of it reports what is on
//! disk — nothing here loads a theme, parses a layaut or opens a
//! plugin. An addon's metadata is read as TEXT for the same reason the
//! desktop reads it as text: describing a widget must never mean
//! running its code.
//!
//! Two things are deliberately NOT invented.
//!
//! The desktop's toolkit carries themes compiled into it, and the
//! desktop links some widgets straight into its binary. Neither is a
//! file, so neither can appear in a scan. Writing the names down here
//! would create a second list to go stale, so the tools say plainly
//! that a listing covers the installed files and not the built-in ones.
//!
//! An addon that declares no label, no category and no heights gets the
//! program's defaults. Those defaults belong to the program, so what is
//! reported here is what the addon actually declares — `None` where it
//! declared nothing — rather than a guess at what the desktop will make
//! of it.

use std::path::{Path, PathBuf};

use crate::tools::error::ToolError;
use crate::tools::paths::{
    confine, safe_component, DesktopDirs, ADDONS_SUB, LAYAUTS_SUB,
};

/// The built-in layaut, which has no file: the toolkit computes it.
pub const BUILTIN_LAYAUT: &str = "default";

/// How far into a script the header pragmas are read. Past this the
/// file is code, and a `// label:` in the middle of it is a comment
/// about the code, not a declaration.
const PRAGMA_LINES: usize = 16;

/// The most a tool result may carry of one file.
pub const MAX_READ_BYTES: u64 = 256 * 1024;

/// An installed theme file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeEntry {
    pub name: String,
    pub path: PathBuf,
}

/// An installed layaut, or the built-in one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayautEntry {
    pub name: String,
    /// `None` for [`BUILTIN_LAYAUT`] when no file overrides it.
    pub path: Option<PathBuf>,
}

/// Which of the two kinds of addon this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddonKind {
    /// A `.rhai` script under `addons/scripts/`.
    Script,
    /// A compiled `.so` under `addons/plugins/`.
    Plugin,
}

impl AddonKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AddonKind::Script => "script",
            AddonKind::Plugin => "plugin",
        }
    }
}

/// An installed addon and what it declares about itself.
#[derive(Clone, Debug, PartialEq)]
pub struct AddonEntry {
    /// The file's stem. This, and not anything in the metadata, is what
    /// the addon is called: a name that could be declared would let one
    /// addon claim to be another.
    pub name: String,
    pub kind: AddonKind,
    pub path: PathBuf,
    pub label: Option<String>,
    pub category: Option<String>,
    pub ref_h: Option<String>,
    pub min_h: Option<String>,
}

/// Every `<name>.theme` on the toolkit's theme search path, first
/// directory holding a name winning, sorted.
pub fn themes(dirs: &DesktopDirs) -> Vec<ThemeEntry> {
    let mut out: Vec<ThemeEntry> = Vec::new();
    for dir in dirs.theme_dirs() {
        for (name, path) in files_with_extension(&dir, "theme") {
            if !out.iter().any(|t| t.name == name) {
                out.push(ThemeEntry { name, path });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The built-in layaut plus every `<name>.layaut` on the data search
/// path, first directory holding a name winning, sorted after the
/// built-in one.
pub fn layauts(dirs: &DesktopDirs) -> Vec<LayautEntry> {
    let mut out = vec![LayautEntry {
        name: BUILTIN_LAYAUT.to_string(),
        path: None,
    }];
    for dir in dirs.asset_dirs(LAYAUTS_SUB) {
        for (name, path) in files_with_extension(&dir, "layaut") {
            match out.iter_mut().find(|l| l.name == name) {
                // An installed `default.layaut` replaces the built-in
                // one, which is what the toolkit does too.
                Some(existing) if existing.path.is_none() => existing.path = Some(path),
                Some(_) => {}
                None => out.push(LayautEntry {
                    name,
                    path: Some(path),
                }),
            }
        }
    }
    out[1..].sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The text of one layaut file.
///
/// `name` comes from a model, so it must be one path component and the
/// file it resolves to must still be inside the data directory it was
/// found in — a `layauts/x.layaut` symlinked to `/etc/shadow` is a
/// refusal, not a read.
pub fn read_layaut(dirs: &DesktopDirs, name: &str) -> Result<Option<(PathBuf, String)>, ToolError> {
    let name = safe_component(name).ok_or_else(|| ToolError::Rejected {
        reason: format!("\"{name}\" is not a layaut name: one path component, no '/' and no '..'"),
    })?;
    for dir in dirs.asset_dirs(LAYAUTS_SUB) {
        let candidate = dir.join(format!("{name}.layaut"));
        if candidate.symlink_metadata().is_err() {
            continue;
        }
        let path = confine(&dir, &candidate)?;
        let size = std::fs::metadata(&path)
            .map_err(|e| ToolError::io(&path, &e))?
            .len();
        if size > MAX_READ_BYTES {
            return Err(ToolError::TooLarge {
                path,
                size,
                limit: MAX_READ_BYTES,
            });
        }
        let text = std::fs::read_to_string(&path).map_err(|e| ToolError::io(&path, &e))?;
        return Ok(Some((path, text)));
    }
    Ok(None)
}

/// Every addon installed under `addons/scripts` and `addons/plugins`,
/// first root holding a name winning, scripts before plugins as the
/// desktop scans them.
pub fn addons(dirs: &DesktopDirs) -> Vec<AddonEntry> {
    let mut out: Vec<AddonEntry> = Vec::new();
    for root in dirs.asset_dirs(ADDONS_SUB) {
        for (sub, ext, kind) in [
            ("scripts", "rhai", AddonKind::Script),
            ("plugins", "so", AddonKind::Plugin),
        ] {
            let mut found = files_with_extension(&root.join(sub), ext);
            // Sorted, so the answer does not depend on the order the
            // filesystem happens to hand directory entries back.
            found.sort();
            for (name, path) in found {
                if out.iter().any(|a| a.name == name) {
                    continue;
                }
                out.push(match kind {
                    AddonKind::Script => from_script(name, path),
                    AddonKind::Plugin => from_meta(name, path),
                });
            }
        }
    }
    out
}

/// A script declares itself in `// key: value` lines in its header.
fn from_script(name: String, path: PathBuf) -> AddonEntry {
    let mut entry = bare(name, AddonKind::Script, path);
    if let Ok(text) = std::fs::read_to_string(&entry.path) {
        for line in text.lines().take(PRAGMA_LINES) {
            let Some(rest) = line.trim().strip_prefix("//") else {
                continue;
            };
            if let Some((k, v)) = rest.split_once(':') {
                declare(&mut entry, k.trim(), v.trim());
            }
        }
    }
    entry
}

/// A compiled plugin declares itself in a `<name>.meta` beside the
/// library. The metadata is read INSTEAD of the library — nothing here
/// dlopens anything.
fn from_meta(name: String, path: PathBuf) -> AddonEntry {
    let mut entry = bare(name, AddonKind::Plugin, path);
    if let Ok(text) = std::fs::read_to_string(entry.path.with_extension("meta")) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                declare(&mut entry, k.trim(), v.trim());
            }
        }
    }
    entry
}

fn bare(name: String, kind: AddonKind, path: PathBuf) -> AddonEntry {
    AddonEntry {
        name,
        kind,
        path,
        label: None,
        category: None,
        ref_h: None,
        min_h: None,
    }
}

/// One declared key. Unknown keys are ignored: an addon written for a
/// later version of the desktop must still be listable by this one.
fn declare(entry: &mut AddonEntry, key: &str, value: &str) {
    let value = (!value.is_empty()).then(|| value.to_string());
    match key {
        "label" => entry.label = value,
        "category" => entry.category = value,
        "ref_h" => entry.ref_h = value,
        "min_h" => entry.min_h = value,
        _ => {}
    }
}

/// `(stem, path)` for every file in `dir` with that extension. Dotfiles
/// are skipped: they are bookkeeping, not installed things.
fn files_with_extension(dir: &Path, ext: &str) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some(ext) {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if stem.starts_with('.') {
            continue;
        }
        let Some(name) = safe_component(stem) else {
            continue;
        };
        out.push((name, path));
    }
    out.sort();
    out
}
