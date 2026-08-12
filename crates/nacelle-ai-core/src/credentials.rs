//! Where the agent's credential comes from, and what each kind of
//! credential means on the wire.
//!
//! Two kinds exist and they are **not** interchangeable:
//!
//! | kind | headers |
//! |---|---|
//! | API key | `x-api-key: <key>` |
//! | OAuth token | `authorization: Bearer <token>` **and** `anthropic-beta: oauth-2025-04-20` |
//!
//! Sending an OAuth token as `x-api-key` fails with a 401 that says
//! nothing useful, so the difference is carried by the type itself:
//! there is no way to hold a credential without also knowing which one
//! it is, and [`Credential::auth_headers`] is the only place the choice
//! is made. A caller cannot get it wrong because a caller never decides.
//!
//! Resolution order is: `ANTHROPIC_API_KEY`, then
//! `ANTHROPIC_AUTH_TOKEN`, then `$XDG_CONFIG_HOME/nacelle-ai/credentials.json`.
//!
//! The secret is never logged, never printed, never put in an error and
//! never written into this repository. [`Secret`]'s `Debug` and
//! `Display` are the single chokepoint for that: every type here that
//! holds a secret holds it in a `Secret`, so redaction cannot be
//! forgotten in one of them.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// Checked first: an API key, used as `x-api-key`.
pub const ENV_API_KEY: &str = "ANTHROPIC_API_KEY";
/// Checked second: an OAuth token, used as `Authorization: Bearer`.
pub const ENV_AUTH_TOKEN: &str = "ANTHROPIC_AUTH_TOKEN";
pub const ENV_XDG_CONFIG_HOME: &str = "XDG_CONFIG_HOME";
pub const ENV_HOME: &str = "HOME";

/// The agent's directory under the user's configuration root.
pub const CONFIG_DIR: &str = "nacelle-ai";
/// The one file in it that may hold a secret.
pub const CONFIG_FILE: &str = "credentials.json";
/// The two keys that file may carry a secret under, one per kind.
pub const FIELD_API_KEY: &str = "api_key";
pub const FIELD_OAUTH_TOKEN: &str = "oauth_token";

pub const HEADER_API_KEY: &str = "x-api-key";
pub const HEADER_AUTHORIZATION: &str = "authorization";
pub const HEADER_ANTHROPIC_BETA: &str = "anthropic-beta";
/// The beta flag an OAuth token has to be accompanied by. Without it the
/// messages endpoint rejects the token even though the bearer header is
/// correct.
pub const OAUTH_BETA: &str = "oauth-2025-04-20";

/// Any permission bit outside the owner's makes a credentials file
/// unusable: group and world must have nothing.
const INSECURE_BITS: u32 = 0o077;
/// What the file is required to be.
const REQUIRED_MODE: u32 = 0o600;

/// A string that must not be shown.
///
/// The value is reachable only through [`Secret::expose`], which is
/// deliberately ugly to read at a call site: every use is a place worth
/// looking at twice. `Debug` and `Display` print a placeholder, so a
/// secret cannot reach a log through `{}`, `{:?}`, `dbg!`, a panic
/// message or a derived `Debug` on any type containing one.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }

    /// The credential itself. Only an outgoing request header should
    /// ever call this.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Which kind of credential this is — safe to print anywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialKind {
    ApiKey,
    OAuth,
}

impl fmt::Display for CredentialKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CredentialKind::ApiKey => f.write_str("API key"),
            CredentialKind::OAuth => f.write_str("OAuth token"),
        }
    }
}

/// A credential, together with the fact of which sort it is.
///
/// `Debug` shows the kind and nothing else, because the secret is held
/// in a [`Secret`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Credential {
    ApiKey(Secret),
    OAuth(Secret),
}

impl Credential {
    pub fn api_key(value: impl Into<String>) -> Self {
        Credential::ApiKey(Secret::new(value))
    }

    pub fn oauth(value: impl Into<String>) -> Self {
        Credential::OAuth(Secret::new(value))
    }

    pub fn kind(&self) -> CredentialKind {
        match self {
            Credential::ApiKey(_) => CredentialKind::ApiKey,
            Credential::OAuth(_) => CredentialKind::OAuth,
        }
    }

    /// The headers this credential authenticates with.
    ///
    /// A backend that needs beta flags of its own must **merge** them
    /// into the `anthropic-beta` value returned here, comma-separated,
    /// rather than sending a second header of the same name — the
    /// endpoint reads only one.
    pub fn auth_headers(&self) -> Vec<(&'static str, String)> {
        match self {
            Credential::ApiKey(secret) => {
                vec![(HEADER_API_KEY, secret.expose().to_string())]
            }
            Credential::OAuth(secret) => vec![
                (
                    HEADER_AUTHORIZATION,
                    format!("Bearer {}", secret.expose()),
                ),
                (HEADER_ANTHROPIC_BETA, OAUTH_BETA.to_string()),
            ],
        }
    }
}

/// Where a credential was found. Carries no secret, so it is safe to
/// show the user — and worth showing, because "which token am I even
/// using" is the first question when a request is rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    Env(&'static str),
    File(PathBuf),
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::Env(name) => write!(f, "environment variable {name}"),
            Origin::File(path) => write!(f, "{}", path.display()),
        }
    }
}

/// A credential and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub credential: Credential,
    pub origin: Origin,
}

/// A source of environment variables.
///
/// Resolution reads the environment through this rather than calling
/// [`std::env::var`] directly, so a test can hand it an exact map and a
/// temporary directory instead of mutating the process — which is both
/// racy across threads and a way for a developer's own real token to
/// wander into a test run.
pub trait Env {
    fn var(&self, key: &str) -> Option<String>;
}

/// The real process environment.
pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

impl Env for HashMap<String, String> {
    fn var(&self, key: &str) -> Option<String> {
        self.get(key).cloned()
    }
}

/// Why no credential could be produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialError {
    /// Nothing was set anywhere. `looked_in` is the config file path
    /// that was checked, when there was one to check.
    Missing { looked_in: Option<PathBuf> },
    /// The file exists but is readable by someone other than its owner,
    /// so it is refused rather than used. Anything that can read the
    /// file can spend the token.
    InsecurePermissions { path: PathBuf, mode: u32 },
    /// The file could not be opened or read.
    Unreadable { path: PathBuf, reason: String },
    /// The file is not the JSON this expects.
    Malformed { path: PathBuf, reason: String },
    /// The file parsed but names no credential.
    Empty { path: PathBuf },
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CredentialError::Missing { looked_in } => {
                write!(
                    f,
                    "no credential: set {ENV_API_KEY} or {ENV_AUTH_TOKEN}"
                )?;
                match looked_in {
                    Some(path) => write!(f, ", or write {}", path.display()),
                    None => Ok(()),
                }
            }
            CredentialError::InsecurePermissions { path, mode } => write!(
                f,
                "refusing to read {}: mode is {:03o} and must be {:03o} \
                 — anyone who can read it can spend the token. Run: chmod {:03o} {}",
                path.display(),
                mode,
                REQUIRED_MODE,
                REQUIRED_MODE,
                path.display()
            ),
            CredentialError::Unreadable { path, reason } => {
                write!(f, "cannot read {}: {reason}", path.display())
            }
            CredentialError::Malformed { path, reason } => {
                write!(f, "cannot parse {}: {reason}", path.display())
            }
            CredentialError::Empty { path } => write!(
                f,
                "{} names no credential: expected \"{FIELD_API_KEY}\" or \"{FIELD_OAUTH_TOKEN}\"",
                path.display()
            ),
        }
    }
}

impl Error for CredentialError {}

/// Where the credentials file lives for this environment, or `None` when
/// the environment says nothing about where the user's configuration is.
pub fn config_path(env: &dyn Env) -> Option<PathBuf> {
    let root = match non_blank(env.var(ENV_XDG_CONFIG_HOME)) {
        Some(xdg) => PathBuf::from(xdg),
        // The XDG default when the variable is unset. Falling back to it
        // matters more than it looks: plenty of sessions never export
        // XDG_CONFIG_HOME, and a user who wrote the file where every
        // other program keeps its config should not be told there is no
        // credential.
        None => PathBuf::from(non_blank(env.var(ENV_HOME))?).join(".config"),
    };
    Some(root.join(CONFIG_DIR).join(CONFIG_FILE))
}

/// Find a credential: environment first, then the configuration file.
///
/// A file whose permissions are too wide is an error, not a miss — the
/// user asked for that file to be used, and quietly ignoring it would
/// leave them wondering why their token does nothing.
pub fn resolve(env: &dyn Env) -> Result<Resolved, CredentialError> {
    if let Some(key) = non_blank(env.var(ENV_API_KEY)) {
        return Ok(Resolved {
            credential: Credential::api_key(key),
            origin: Origin::Env(ENV_API_KEY),
        });
    }

    if let Some(token) = non_blank(env.var(ENV_AUTH_TOKEN)) {
        return Ok(Resolved {
            credential: Credential::oauth(token),
            origin: Origin::Env(ENV_AUTH_TOKEN),
        });
    }

    let path = match config_path(env) {
        Some(path) => path,
        None => return Err(CredentialError::Missing { looked_in: None }),
    };

    if !path.exists() {
        return Err(CredentialError::Missing {
            looked_in: Some(path),
        });
    }

    let credential = load_file(&path)?;
    Ok(Resolved {
        credential,
        origin: Origin::File(path),
    })
}

/// Read a credential from one named file, permissions checked.
///
/// Exposed on its own so a caller can point at a path directly, and so
/// tests never have to guess where resolution would have looked.
pub fn load_file(path: &Path) -> Result<Credential, CredentialError> {
    let meta = fs::metadata(path).map_err(|err| CredentialError::Unreadable {
        path: path.to_path_buf(),
        reason: err.to_string(),
    })?;

    // Checked before the read, not after: a file the whole machine can
    // read is one we would rather never have had in this process's
    // memory at all.
    check_permissions(path, &meta)?;

    let text = fs::read_to_string(path).map_err(|err| CredentialError::Unreadable {
        path: path.to_path_buf(),
        reason: err.to_string(),
    })?;

    // Read as a plain document rather than through a derived type. Two
    // optional strings do not justify pulling a derive macro — and its
    // proc-macro dependency chain — into a crate whose dependencies have
    // to be licence-audited one by one.
    let document: serde_json::Value =
        serde_json::from_str(&text).map_err(|err| CredentialError::Malformed {
            path: path.to_path_buf(),
            // serde_json reports a position and a type, never the input
            // itself, so this cannot echo the secret back out.
            reason: err.to_string(),
        })?;

    let object = document
        .as_object()
        .ok_or_else(|| CredentialError::Malformed {
            path: path.to_path_buf(),
            reason: "expected a JSON object".to_string(),
        })?;

    // Unknown keys are ignored on purpose, so a setting added beside the
    // secret later does not make an older build refuse the file.
    // The order matches the environment: a key beats a token, so a file
    // holding both behaves like an environment holding both.
    for (field, wrap) in [
        (FIELD_API_KEY, Credential::api_key as fn(String) -> Credential),
        (FIELD_OAUTH_TOKEN, Credential::oauth as fn(String) -> Credential),
    ] {
        match object.get(field) {
            None => continue,
            // Present but not a string is a mistake worth naming, rather
            // than something to skip past and then report as "empty".
            Some(value) if !value.is_string() => {
                return Err(CredentialError::Malformed {
                    path: path.to_path_buf(),
                    reason: format!("\"{field}\" must be a string"),
                })
            }
            Some(value) => {
                if let Some(secret) = non_blank(value.as_str().map(str::to_string)) {
                    return Ok(wrap(secret));
                }
            }
        }
    }

    Err(CredentialError::Empty {
        path: path.to_path_buf(),
    })
}

#[cfg(unix)]
fn check_permissions(path: &Path, meta: &fs::Metadata) -> Result<(), CredentialError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = meta.permissions().mode() & 0o777;
    if mode & INSECURE_BITS != 0 {
        return Err(CredentialError::InsecurePermissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

/// Other platforms have a different permission model entirely, so there
/// is nothing honest to check here yet. The agent runs on Linux today;
/// when it does not, this is the place that has to grow.
#[cfg(not(unix))]
fn check_permissions(_path: &Path, _meta: &fs::Metadata) -> Result<(), CredentialError> {
    Ok(())
}

/// Trim, and treat what is left of an empty value as absent.
///
/// A leftover `export ANTHROPIC_API_KEY=` in a shell profile is common,
/// and treating it as "a credential that happens to be empty" would
/// shadow the working token underneath it and produce a 401 nobody can
/// explain. Trailing whitespace gets the same treatment: a token written
/// with `echo` ends in a newline that no header should carry.
fn non_blank(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
