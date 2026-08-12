//! Layer 1: the files this program never opens.
//!
//! The strongest thing that can be said about a secret is that the
//! program never read it. Every layer after this one works on bytes that
//! are already in memory; this one is the only one that can promise they
//! never were.
//!
//! So the list is a **denylist checked before the read**, on the
//! canonical path — `..` collapsed and every symlink followed — because
//! a link named `theme.layaut` pointing at `~/.ssh/id_ed25519` is the
//! same read as opening the key directly, and only the resolved path
//! shows it.
//!
//! Three properties are the whole point, and each is a thing this module
//! deliberately does not offer:
//!
//! * **It cannot be switched off.** There is no `allow`, no `remove`, no
//!   `Denylist::empty()`, and the fields are private. [`Denylist::also`]
//!   adds; nothing subtracts. A model that asks nicely, a tool argument
//!   that says `force`, and a system prompt that claims to be the
//!   administrator all arrive at the same refusal.
//! * **It refuses out loud.** A [`Denial`] is returned and reported, not
//!   swallowed. The user may well be asking for a good reason — they own
//!   the machine — and a program that quietly declines teaches them
//!   nothing about why their question went unanswered.
//! * **It does not need the model's opinion.** Nothing here asks
//!   anything to judge anything. It is a path comparison and a look at
//!   the first bytes of a file.
//!
//! What is denied, and why each entry is here rather than left to the
//! pattern scan in [`scan`](super::scan): every one of them holds
//! material that is *only* useful to whoever is supposed to have it, and
//! for which there is no version of "the agent read it by accident" that
//! ends well.

use std::fmt;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use crate::credentials::Env;
use crate::tools::error::ToolError;

pub const ENV_HOME: &str = "HOME";
pub const ENV_XDG_CONFIG_HOME: &str = "XDG_CONFIG_HOME";
pub const ENV_XDG_DATA_HOME: &str = "XDG_DATA_HOME";
/// `pass` reads this to find its store; a user who moved theirs should
/// not have it read just because it is no longer where it usually is.
pub const ENV_PASSWORD_STORE: &str = "PASSWORD_STORE_DIR";

/// How much of a file is looked at before deciding whether its contents
/// disqualify it. A PEM armour line is the first line of the file, so
/// this only has to be long enough to hold one — 1 KiB also covers a
/// file that begins with a comment or a byte-order mark.
const SNIFF_BYTES: usize = 1024;

/// Directories under the home directory that are never read, and what
/// each one is.
///
/// Written relative to home rather than absolutely so a test can point
/// the whole list at a throwaway directory, which is also what stops
/// these tests from depending on the developer's own `~/.ssh`.
///
/// Public, like the four lists under it, so that a test can walk the
/// list itself rather than a handful of examples from it. A denylist
/// tested by example is a denylist whose next entry arrives untested,
/// and the entry nobody tested is the one with the typo in it. Reading
/// the list gives nothing away: it says which files are refused, which
/// is what [`Denial`] says out loud anyway.
pub const HOME_DIRS: &[(&str, Reason)] = &[
    (".ssh", Reason::CredentialStore),
    (".gnupg", Reason::CredentialStore),
    (".aws", Reason::CredentialStore),
    (".azure", Reason::CredentialStore),
    (".kube", Reason::CredentialStore),
    (".docker", Reason::CredentialStore),
    // Not a directory, but it is matched the same way and it is a
    // credential store in every sense that matters: host, login,
    // password, in plain text.
    (".netrc", Reason::CredentialStore),
    (".password-store", Reason::PasswordStore),
    // The certificate and key databases NSS keeps for Chrome, Evolution
    // and everything else built on it, and the keyring GNOME wrote
    // before it moved under the data directory.
    (".pki", Reason::CredentialStore),
    (".gnome2/keyrings", Reason::CredentialStore),
    // Files rather than directories, and every one of them holds a
    // password or a token in plain text. They are here rather than left
    // to the pattern scan for the reason the module header gives: what
    // is never read cannot leak, and none of these has a version of "the
    // agent opened it by accident" that ends well.
    //
    //   .git-credentials       git's own store, one URL with the
    //                          password in it per line
    //   .npmrc, .pypirc        a registry token and an upload password
    //   .pgpass, .my.cnf       database passwords, read by the client
    //                          without anybody being asked
    //   .cargo/credentials*    the crates.io token — this very workspace
    //                          is published with it
    //   .gem/credentials       the same for RubyGems
    (".git-credentials", Reason::CredentialStore),
    (".npmrc", Reason::CredentialStore),
    (".pypirc", Reason::CredentialStore),
    (".pgpass", Reason::CredentialStore),
    (".my.cnf", Reason::CredentialStore),
    (".cargo/credentials", Reason::CredentialStore),
    (".cargo/credentials.toml", Reason::CredentialStore),
    (".gem/credentials", Reason::CredentialStore),
    // Infrastructure tooling that writes long-lived tokens next to its
    // configuration.
    (".terraform.d", Reason::CredentialStore),
    (".subversion/auth", Reason::CredentialStore),
    // The other agent on this machine. Its token is a credential in
    // exactly the way this program's own is, and the argument in
    // CONFIG_DIRS for `nacelle-ai` is the same argument.
    (".claude/.credentials.json", Reason::CredentialStore),
    (".mozilla", Reason::BrowserProfile),
    (".thunderbird", Reason::BrowserProfile),
    // Flatpak and snap keep the same profiles somewhere else entirely,
    // and a denylist that only knew the distribution packaging would be
    // a denylist with a hole in it on most current desktops.
    (".var/app/org.mozilla.firefox", Reason::BrowserProfile),
    (".var/app/com.google.Chrome", Reason::BrowserProfile),
    (".var/app/com.brave.Browser", Reason::BrowserProfile),
    (".var/app/org.chromium.Chromium", Reason::BrowserProfile),
    (".var/app/com.microsoft.Edge", Reason::BrowserProfile),
    ("snap/firefox", Reason::BrowserProfile),
    ("snap/chromium", Reason::BrowserProfile),
];

/// The same, relative to the configuration root — `$XDG_CONFIG_HOME`
/// when it is set, and `$HOME/.config` in any case, because a user who
/// exports the variable still has files under the default from before
/// they did.
pub const CONFIG_DIRS: &[(&str, Reason)] = &[
    ("gh", Reason::CredentialStore),
    ("gcloud", Reason::CredentialStore),
    ("anthropic", Reason::CredentialStore),
    ("op", Reason::PasswordStore),
    // Named as files, not directories, because the rest of what these
    // programs keep beside them is ordinary configuration and refusing
    // it would be a refusal the user cannot act on.
    ("git/credentials", Reason::CredentialStore),
    ("containers/auth.json", Reason::CredentialStore),
    // Whole directories, because everything in them is key material or a
    // token: rclone writes OAuth refresh tokens into its one file, sops
    // keeps the age keys that decrypt everything else, and Bitwarden's
    // desktop client keeps the vault and its session there.
    ("rclone", Reason::CredentialStore),
    ("sops", Reason::CredentialStore),
    ("age", Reason::CredentialStore),
    ("Bitwarden", Reason::PasswordStore),
    // This program's own token lives here. An agent that could read its
    // own credentials file could put the token in a reply, and a reply
    // is exactly what crosses the network.
    ("nacelle-ai", Reason::CredentialStore),
    ("google-chrome", Reason::BrowserProfile),
    ("chromium", Reason::BrowserProfile),
    ("BraveSoftware", Reason::BrowserProfile),
    ("microsoft-edge", Reason::BrowserProfile),
    ("vivaldi", Reason::BrowserProfile),
    ("opera", Reason::BrowserProfile),
    ("librewolf", Reason::BrowserProfile),
];

/// And relative to the data root.
///
/// The two histories here are histories in a database rather than in a
/// text file, so the name rules below cannot see them: atuin keeps
/// `history.db` and nushell `history.txt` or `history.sqlite3`, and
/// neither name is one this program can refuse everywhere without
/// refusing somebody's changelog. The directory is what makes them
/// unambiguous.
pub const DATA_DIRS: &[(&str, Reason)] = &[
    ("keyrings", Reason::CredentialStore),
    ("atuin", Reason::ShellHistory),
    ("nushell", Reason::ShellHistory),
];

/// Extensions that are key or certificate material whatever they are
/// called and wherever they are.
pub const KEY_EXTENSIONS: &[&str] = &[
    "pem", "key", "p12", "pfx", "jks", "keystore", "kdbx", "ppk", "gpg", "asc", "agekey",
];

/// Extensions an SSH private key is allowed to have. `id_*` on its own
/// would refuse `id_card.png` and any layaut somebody named `id_two`,
/// which is a refusal the user cannot act on and would not understand;
/// the shapes below are the ones `ssh-keygen` actually writes.
const ID_EXTENSIONS: &[&str] = &["pub", "pem", "key"];

/// Files whose whole content is a record of what the user typed.
///
/// The `*_history` suffix rule below covers `.bash_history`,
/// `.zsh_history`, `.psql_history` and the rest of that family. These
/// are the ones that do not follow it: zsh writes `.histfile` when
/// `HISTFILE` is set the short way, and `.zhistory` on macOS-flavoured
/// setups.
///
/// `.Rhistory` and `.dbshell` are on the list for the same reason and
/// were missing from it. R writes the first — every expression typed at
/// its prompt, and a data scientist's prompt is where the connection
/// string goes — and it has no underscore, so no suffix rule sees it.
/// The mongo shell writes the second, under a name that says nothing at
/// all about what it holds, which is every query and every
/// `db.auth(user, password)` line.
pub const HISTORY_NAMES: &[&str] = &[
    ".history",
    ".histfile",
    ".zhistory",
    ".rhistory",
    ".dbshell",
    ".lesshst",
    ".viminfo",
    ".sqlite_history",
];

/// Why a path is refused.
///
/// One sentence each, addressed to the user rather than to a log: they
/// are the ones who will see it, and they are entitled to know which
/// rule stood in the way of the question they asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// A store of logins, keys or tokens for other programs.
    CredentialStore,
    /// A password manager's database.
    PasswordStore,
    /// A browser profile: cookies, saved logins, session tokens.
    BrowserProfile,
    /// A `.env` file. They exist to hold the values that must not be in
    /// the source tree, which is exactly the set worth refusing.
    Environment,
    /// Key or certificate material, by name.
    KeyMaterial,
    /// Key material, by content: the file begins with PEM armour.
    KeyArmor,
    /// A shell or interpreter history — every command the user typed,
    /// including the ones with a token in them.
    ShellHistory,
}

impl Reason {
    /// The sentence shown to the user.
    pub fn why(&self) -> &'static str {
        match self {
            Reason::CredentialStore => {
                "it is a credential store — keys and tokens for other programs live there"
            }
            Reason::PasswordStore => "it belongs to a password manager",
            Reason::BrowserProfile => {
                "it is a browser profile, which holds cookies and saved logins"
            }
            Reason::Environment => {
                "a .env file exists to hold the values that must not be in the source tree"
            }
            Reason::KeyMaterial => "it is key or certificate material",
            Reason::KeyArmor => "it begins with a PEM private-key header",
            Reason::ShellHistory => {
                "it is a command history, which records everything typed at a prompt"
            }
        }
    }
}

/// A read that did not happen.
///
/// Carries the path, because the user is owed the name of the file that
/// was refused, and never a byte of its contents — the whole point is
/// that there are none to carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denial {
    /// What the caller asked for, as it was written.
    pub path: PathBuf,
    /// The rule that matched: a directory, an extension, a name.
    pub matched: String,
    pub reason: Reason,
}

impl fmt::Display for Denial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "refused to read {}: {} (matched {}). This refusal is not something a \
             prompt or a tool argument can lift — if you need what is in that file, \
             open it yourself.",
            self.path.display(),
            self.reason.why(),
            self.matched
        )
    }
}

impl std::error::Error for Denial {}

impl From<Denial> for ToolError {
    fn from(denial: Denial) -> Self {
        ToolError::Denied {
            path: denial.path.clone(),
            why: denial.to_string(),
        }
    }
}

/// The list, and the only sanctioned way to read a file in this crate.
///
/// Cheap to clone: a handful of paths, built once per session and passed
/// wherever a read happens. It is a value rather than a global precisely
/// so that a read cannot be written that forgot to consult it — the
/// guard is a parameter, and a function without one cannot read.
#[derive(Clone, Debug)]
pub struct Denylist {
    /// Absolute, canonical where the directory exists. Each with the
    /// reason it is on the list.
    dirs: Vec<(PathBuf, Reason)>,
}

impl Denylist {
    /// The list for a home directory, or — with `None` — the rules that
    /// need no home at all.
    ///
    /// `None` is not a disabled denylist. The name, extension and
    /// content rules are absolute and still apply; only the directory
    /// rules, which are the ones written relative to a home, are absent.
    /// That is the honest answer for an environment that names no home:
    /// refusing everything would be useless, and pretending `~` is `/`
    /// would be worse.
    pub fn new(home: Option<&Path>) -> Self {
        let mut list = Denylist { dirs: Vec::new() };
        let Some(home) = home else {
            return list;
        };
        for (relative, reason) in HOME_DIRS {
            list.push(home.join(relative), *reason);
        }
        for (relative, reason) in CONFIG_DIRS {
            list.push(home.join(".config").join(relative), *reason);
        }
        for (relative, reason) in DATA_DIRS {
            list.push(home.join(".local/share").join(relative), *reason);
        }
        list
    }

    /// The list this environment describes.
    ///
    /// `XDG_CONFIG_HOME` and `XDG_DATA_HOME` are honoured *in addition
    /// to* the defaults rather than instead of them: a user who exports
    /// the variable today still has the files they wrote under
    /// `~/.config` yesterday, and a denylist is the wrong place to be
    /// clever about which one is in force.
    pub fn from_env(env: &dyn Env) -> Self {
        let home = non_blank(env.var(ENV_HOME)).map(PathBuf::from);
        let mut list = Denylist::new(home.as_deref());

        if let Some(config) = non_blank(env.var(ENV_XDG_CONFIG_HOME)) {
            for (relative, reason) in CONFIG_DIRS {
                list.push(PathBuf::from(&config).join(relative), *reason);
            }
        }
        if let Some(data) = non_blank(env.var(ENV_XDG_DATA_HOME)) {
            for (relative, reason) in DATA_DIRS {
                list.push(PathBuf::from(&data).join(relative), *reason);
            }
        }
        if let Some(store) = non_blank(env.var(ENV_PASSWORD_STORE)) {
            list.push(PathBuf::from(store), Reason::PasswordStore);
        }
        list
    }

    /// Deny one more directory.
    ///
    /// The only mutation this type has, and it only ever narrows what
    /// may be read. There is no matching removal, and there will not be
    /// one: a list that can be shortened at runtime is a list an
    /// argument can shorten.
    pub fn also(mut self, dir: impl Into<PathBuf>, reason: Reason) -> Self {
        self.push(dir.into(), reason);
        self
    }

    /// Every directory on the list, for a user asking what it covers.
    pub fn directories(&self) -> Vec<&Path> {
        self.dirs.iter().map(|(p, _)| p.as_path()).collect()
    }

    /// May this path be opened? Answers with the resolved path, so a
    /// caller reads the file this checked and not the one it was handed.
    ///
    /// The content rule cannot be applied here — there are no contents
    /// yet — which is why [`Denylist::read_to_string`] exists and why it
    /// is what the rest of the crate calls.
    pub fn check(&self, path: &Path) -> Result<PathBuf, Denial> {
        let resolved = canonical(path);

        for (dir, reason) in &self.dirs {
            if resolved.starts_with(dir) {
                return Err(Denial {
                    path: path.to_path_buf(),
                    matched: dir.display().to_string(),
                    reason: *reason,
                });
            }
        }

        // Checked on the resolved name as well as the written one: a
        // link called `notes.txt` pointing at `.env` is a read of
        // `.env`, and the name the caller used says nothing about it.
        for name in [resolved.file_name(), path.file_name()].into_iter().flatten() {
            if let Some((matched, reason)) = name_rule(&name.to_string_lossy()) {
                return Err(Denial {
                    path: path.to_path_buf(),
                    matched,
                    reason,
                });
            }
        }

        Ok(resolved)
    }

    /// Read a file, or say why not.
    ///
    /// This is layer 1 in one call: the path is checked, the file is
    /// opened, and its first bytes are looked at before any of it is
    /// returned to a caller. A private key that somebody stored as
    /// `notes.txt` is refused here and nowhere else — no pattern scan
    /// downstream can un-read it.
    pub fn read_to_string(&self, path: &Path) -> Result<String, ToolError> {
        let resolved = self.check(path)?;

        let mut file = std::fs::File::open(&resolved).map_err(|e| ToolError::io(&resolved, &e))?;

        // Bytes, not text, and only the head: the sniff has to happen
        // before the rest of the file is read, and it must not be
        // defeatable by a file that is not valid UTF-8.
        let mut bytes = Vec::with_capacity(SNIFF_BYTES);
        (&mut file)
            .take(SNIFF_BYTES as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| ToolError::io(&resolved, &e))?;
        if let Some(matched) = key_armor(&bytes) {
            return Err(Denial {
                path: path.to_path_buf(),
                matched,
                reason: Reason::KeyArmor,
            }
            .into());
        }

        // The rest is appended as bytes and the whole thing decoded
        // once. Decoding the head on its own would fail on any file
        // whose 1024th byte lands in the middle of a character, which is
        // most of a paragraph of Polish.
        file.read_to_end(&mut bytes)
            .map_err(|e| ToolError::io(&resolved, &e))?;
        String::from_utf8(bytes).map_err(|_| ToolError::Io {
            path: resolved.clone(),
            reason: "not UTF-8 text".to_string(),
        })
    }

    fn push(&mut self, dir: PathBuf, reason: Reason) {
        let dir = canonical(&dir);
        if !self.dirs.iter().any(|(known, _)| *known == dir) {
            self.dirs.push((dir, reason));
        }
    }
}

/// The rule one file name breaks, if it breaks one.
fn name_rule(name: &str) -> Option<(String, Reason)> {
    // Compared case-insensitively: `ID_RSA` and `Secret.PEM` are the
    // same files, and a filesystem that preserves case is not a reason
    // to read one of them.
    let lower = name.to_ascii_lowercase();

    if lower == ".env" || lower.starts_with(".env.") || lower.ends_with(".env") {
        return Some((".env".to_string(), Reason::Environment));
    }

    let extension = lower.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
    if KEY_EXTENSIONS.contains(&extension) {
        return Some((format!("*.{extension}"), Reason::KeyMaterial));
    }

    if lower.starts_with("id_")
        && (!lower.contains('.') || ID_EXTENSIONS.contains(&extension))
    {
        return Some(("id_*".to_string(), Reason::KeyMaterial));
    }

    if HISTORY_NAMES.contains(&lower.as_str())
        || lower.ends_with("_history")
        || lower.ends_with("_hist")
        || lower == "fish_history"
    {
        return Some(("shell history".to_string(), Reason::ShellHistory));
    }

    None
}

/// The armour line at the start of a file, if there is one.
///
/// Only private material: a `CERTIFICATE` block is public by
/// construction and refusing it would train the user that the refusals
/// are noise.
fn key_armor(head: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(head);
    for line in text.lines().take(8) {
        let line = line.trim();
        if !line.starts_with("-----BEGIN") {
            continue;
        }
        for what in ["PRIVATE KEY", "OPENSSH PRIVATE KEY", "PGP PRIVATE KEY"] {
            if line.contains(what) {
                return Some(format!("-----BEGIN ... {what}-----"));
            }
        }
    }
    None
}

/// The path with `..` collapsed and every symlink followed.
///
/// A file that does not exist still has to resolve, because a denylist
/// that only worked on existing files would let a write-then-read pair
/// through; its parent is resolved instead and the name joined back on.
/// When even that fails the path is normalised lexically, which is
/// weaker — it cannot see through a symlink — but is never worse than
/// comparing the string as it arrived.
fn canonical(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(real) = parent.canonicalize() {
            return real.join(name);
        }
    }
    lexical(path)
}

/// `.` dropped, `..` applied, relative made absolute against the working
/// directory.
///
/// The working directory is process state, which this crate otherwise
/// avoids — but a relative path has no meaning without it, and reading
/// `id_rsa` from inside `~/.ssh` has to be refused by the directory rule
/// and not only by the name.
fn lexical(path: &Path) -> PathBuf {
    let mut out = match path.is_absolute() {
        true => PathBuf::new(),
        false => std::env::current_dir().unwrap_or_default(),
    };
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn non_blank(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}
