//! Replacing a file without ever leaving half of one behind.
//!
//! Configuration is read by a program that may be starting at the
//! moment it is written, so a plain "open, truncate, write" is not good
//! enough: between the truncate and the last byte, the file on disk is
//! a valid path to an empty or clipped configuration, and a desktop
//! that read it then would come up wrong. The sequence here has no such
//! window.
//!
//! 1. Copy the previous contents to `<name>.bak`. If that fails,
//!    nothing else happens — a change that cannot be undone is not made.
//! 2. Write the new text to a temporary file in the SAME directory (the
//!    same filesystem, so the rename below cannot fail with `EXDEV`)
//!    and `fsync` it, so the name is never published over unflushed
//!    bytes.
//! 3. `rename` the temporary file over the target. On a POSIX
//!    filesystem that is atomic: a reader sees the old file or the new
//!    one, never a mixture, and never a missing file.
//!
//! A failure anywhere after step 2 removes the temporary file and
//! leaves the original exactly as it was.
//!
//! The backup is a single `.bak` beside the file rather than a numbered
//! history: one predictable name a user can find, and the desktop's own
//! configuration is small enough that keeping generations of it would
//! be clutter rather than safety.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::tools::error::ToolError;

/// What a replaced file's previous contents are kept as.
pub const BACKUP_SUFFIX: &str = ".bak";

/// What a replacement did, for the tool result to report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replaced {
    pub path: PathBuf,
    /// `None` when the file did not exist yet: there was nothing to
    /// back up, and saying so is more useful than naming a file that
    /// was never written.
    pub backup: Option<PathBuf>,
}

/// Put `body` in `path`, atomically, keeping what was there.
///
/// `path` is expected to have been confined already — this function is
/// the mechanism, not the policy, and it deliberately does not know
/// which directory the caller is allowed in.
pub fn replace(path: &Path, body: &str) -> Result<Replaced, ToolError> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(ToolError::Rejected {
            reason: format!("{} is not a file path", path.display()),
        });
    };
    let name = name.to_string_lossy().into_owned();

    let backup = if path.is_file() {
        let backup = dir.join(format!("{name}{BACKUP_SUFFIX}"));
        fs::copy(path, &backup).map_err(|e| ToolError::io(&backup, &e))?;
        Some(backup)
    } else {
        None
    };

    // Hidden, so a crash between the create and the rename does not
    // leave something that looks like a configuration file, and unique
    // per process and call, so two agents writing at once cannot land
    // on the same temporary name.
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let tmp = dir.join(format!(
        ".{name}.tmp.{}.{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));

    // A replacement must not change who may read the file, in either
    // direction: the new file inherits the old file's mode instead of
    // whatever the umask would have given it.
    let mode = existing_mode(path);
    let written = write_and_rename(&tmp, path, body, mode);
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(ToolError::io(path, &e));
    }
    // The rename is durable once the directory entry is on disk. Best
    // effort: a filesystem that refuses to fsync a directory is not a
    // reason to report a write that did happen as a failure.
    if let Ok(handle) = fs::File::open(dir) {
        let _ = handle.sync_all();
    }
    Ok(Replaced {
        path: path.to_path_buf(),
        backup,
    })
}

fn write_and_rename(
    tmp: &Path,
    path: &Path,
    body: &str,
    mode: Option<u32>,
) -> std::io::Result<()> {
    let mut file = fs::File::create(tmp)?;
    file.write_all(body.as_bytes())?;
    file.sync_all()?;
    drop(file);
    if let Some(mode) = mode {
        set_mode(tmp, mode)?;
    }
    fs::rename(tmp, path)
}

#[cfg(unix)]
fn existing_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).ok().map(|m| m.permissions().mode())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn existing_mode(_path: &Path) -> Option<u32> {
    None
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}
