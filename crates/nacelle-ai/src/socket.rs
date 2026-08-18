//! Where the socket lives and who may open it.
//!
//! The spec (`.gap-program/decyzja-nacelle-ai-daemon.md`) names the
//! place exactly, and the client fleet computes the same path from the
//! same page, so nothing here may improvise:
//!
//! ```text
//! $XDG_RUNTIME_DIR/nacelle/ai.sock      (directory 0700, socket 0600)
//! /tmp/nacelle-$UID/ai.sock             when XDG_RUNTIME_DIR is unset
//! ```
//!
//! The permissions are the boundary: everything crossing this socket is
//! the owner's conversation with their own machine, and `0700` on the
//! directory plus `0600` on the socket is what makes "the owner" mean
//! the Unix user and nobody else. They are set unconditionally, even on
//! a directory that already existed — a directory another program
//! loosened does not get to loosen this one's socket.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// The socket's file name, fixed by the spec.
pub const SOCKET_NAME: &str = "ai.sock";

/// Where the session's runtime directory is, when there is a session.
pub const RUNTIME_DIR_ENV: &str = "XDG_RUNTIME_DIR";

/// The directory the socket lives in, given the environment.
///
/// Takes the environment as a function for the same reason
/// [`media::Ffmpeg::find`](crate::media::Ffmpeg::find) does: the tests
/// hand in a table, the daemon hands in [`std::env::var`], and the rule
/// itself stays a pure function that can be checked without touching
/// the process environment.
pub fn place(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf, String> {
    if let Some(runtime) = env(RUNTIME_DIR_ENV).filter(|v| !v.trim().is_empty()) {
        return Ok(Path::new(runtime.trim()).join("nacelle"));
    }
    // The spec says `/tmp/nacelle-$UID` in as many words — not the
    // platform's idea of a temp dir — because the client computes the
    // same string and the two must meet.
    let uid = uid().ok_or_else(|| {
        "cannot place the socket: XDG_RUNTIME_DIR is unset and the user id cannot be read"
            .to_string()
    })?;
    Ok(PathBuf::from("/tmp").join(format!("nacelle-{uid}")))
}

/// This process's user id, read off a file that is always ours.
///
/// `/proc/self` is owned by the process's own user on Linux, which is
/// the one platform this daemon runs on; reading its metadata is a uid
/// syscall without a libc dependency.
fn uid() -> Option<u32> {
    fs::metadata("/proc/self").ok().map(|m| m.uid())
}

/// Make the directory ours alone, put the socket in it, and listen.
///
/// A socket file already standing there is either a daemon that is
/// alive — connecting to it succeeds, and this one refuses to start
/// over it — or the remains of one that was killed, which are removed.
/// That is the one piece of tidying the daemon does unasked, and it is
/// about its own front door, not the user's files.
pub fn listen(dir: &Path) -> Result<(UnixListener, PathBuf), String> {
    let mut build = fs::DirBuilder::new();
    build.recursive(true);
    build.mode(0o700);
    build
        .create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    // `create` leaves an existing directory's mode alone; this does not.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("cannot make {} private: {e}", dir.display()))?;

    bind(&dir.join(SOCKET_NAME))
}

/// Listen on a socket the CONFIGURATION named, rather than on the one
/// this program places itself.
///
/// The difference from [`listen`] is what happens to the directory, and
/// it is deliberate: [`listen`] owns `<runtime>/nacelle` and sets it
/// `0700` every time, because it made it and nothing else lives there.
/// A directory somebody named in `nacelle-ai.ron` is THEIRS — it may be
/// shared, it may be a place with other things in it — and a daemon
/// that quietly took `/tmp` down to `0700` on the way past would break
/// the machine to tighten itself. So the directory must already exist,
/// its mode is left exactly as it is, and the `0600` on the socket
/// itself is unchanged.
///
/// A named socket in a directory others can enter is therefore a
/// weaker position than the standard place, and knowingly so: the file
/// is still the user's alone, but its NAME is visible. The socket's own
/// security — who may connect and what they may then command — is the
/// owner's deferred pass, not this function's.
pub fn listen_named(path: &Path) -> Result<(UnixListener, PathBuf), String> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("{} is not a place a socket can go", path.display()))?;
    if !parent.is_dir() {
        return Err(format!(
            "{} names a socket in {}, which is not a directory \u{2014} this daemon creates the \
             standard place and no other",
            path.display(),
            parent.display()
        ));
    }
    bind(path)
}

/// Put the socket at `path` and listen on it, sweeping the remains of a
/// daemon that was killed and refusing to stand where a live one
/// already answers.
fn bind(path: &Path) -> Result<(UnixListener, PathBuf), String> {
    let path = path.to_path_buf();
    if path.symlink_metadata().is_ok() {
        match UnixStream::connect(&path) {
            Ok(_) => {
                return Err(format!(
                    "another nacelle-ai is already listening on {}",
                    path.display()
                ))
            }
            Err(_) => {
                fs::remove_file(&path)
                    .map_err(|e| format!("cannot remove the stale socket {}: {e}", path.display()))?;
            }
        }
    }

    let listener = UnixListener::bind(&path)
        .map_err(|e| format!("cannot listen on {}: {e}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("cannot make {} private: {e}", path.display()))?;
    Ok((listener, path))
}

/// [`place`] and [`listen`] in one motion, from a real environment.
pub fn listen_from_env(
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(UnixListener, PathBuf), String> {
    listen(&place(env)?)
}

// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runtime_dir_wins_when_it_is_set() {
        let env = |name: &str| {
            (name == RUNTIME_DIR_ENV).then(|| "/run/user/1000".to_string())
        };
        assert_eq!(
            place(&env).unwrap(),
            PathBuf::from("/run/user/1000/nacelle")
        );
    }

    #[test]
    fn a_blank_runtime_dir_counts_as_unset() {
        let env = |name: &str| (name == RUNTIME_DIR_ENV).then(|| "  ".to_string());
        let fallen = place(&env).unwrap();
        let uid = uid().expect("this test runs on Linux, where /proc/self exists");
        assert_eq!(fallen, PathBuf::from(format!("/tmp/nacelle-{uid}")));
    }

    #[test]
    fn the_fallback_is_the_spec_string_not_the_platform_temp_dir() {
        let fallen = place(&|_| None).unwrap();
        assert!(fallen.starts_with("/tmp"));
        assert!(fallen
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("nacelle-"));
    }
}
