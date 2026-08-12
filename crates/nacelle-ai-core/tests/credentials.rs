//! Credential resolution, end to end and hermetically.
//!
//! Nothing here touches the process environment or the real
//! `$XDG_CONFIG_HOME`: every test builds an explicit variable map and a
//! throwaway directory. That keeps the tests order-independent under
//! `cargo test`'s thread pool, and it means a developer's own token can
//! never take part in a test run.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use nacelle_ai::credentials::{
    self, Credential, CredentialError, CredentialKind, Origin, Secret, CONFIG_DIR, CONFIG_FILE,
    ENV_API_KEY, ENV_AUTH_TOKEN, ENV_HOME, ENV_XDG_CONFIG_HOME, HEADER_ANTHROPIC_BETA,
    HEADER_API_KEY, HEADER_AUTHORIZATION, OAUTH_BETA,
};

/// A value distinctive enough that a leak into any string is
/// unmistakable, and that no assertion could match by accident.
const SECRET: &str = "sk-ant-TESTSECRET-do-not-leak-0123456789";

fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

/// A fresh directory for one test. The counter keeps parallel tests from
/// colliding without needing a lock.
fn scratch(tag: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "nacelle-ai-test-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// Write `body` where resolution will look, with the given mode.
fn write_config(root: &Path, body: &str, mode: u32) -> PathBuf {
    let dir = root.join(CONFIG_DIR);
    fs::create_dir_all(&dir).expect("config directory");
    let path = dir.join(CONFIG_FILE);
    fs::write(&path, body).expect("write config");
    set_mode(&path, mode);
    path
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("set mode");
}

/// The default umask would leave a fresh file group- and world-readable,
/// so every test file has to be chmodded explicitly — which is exactly
/// the condition [`CredentialError::InsecurePermissions`] exists to
/// catch.
#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

#[test]
fn api_key_env_beats_everything_else() {
    let root = scratch("order");
    write_config(&root, r#"{"api_key": "from-file"}"#, 0o600);

    let vars = env(&[
        (ENV_API_KEY, SECRET),
        (ENV_AUTH_TOKEN, "from-token-var"),
        (ENV_XDG_CONFIG_HOME, root.to_str().unwrap()),
    ]);

    let resolved = credentials::resolve(&vars).expect("resolve");
    assert_eq!(resolved.credential.kind(), CredentialKind::ApiKey);
    assert_eq!(resolved.origin, Origin::Env(ENV_API_KEY));

    fs::remove_dir_all(&root).ok();
}

#[test]
fn auth_token_is_an_oauth_credential() {
    let root = scratch("token");
    let vars = env(&[
        (ENV_AUTH_TOKEN, SECRET),
        (ENV_XDG_CONFIG_HOME, root.to_str().unwrap()),
    ]);

    let resolved = credentials::resolve(&vars).expect("resolve");
    assert_eq!(resolved.credential.kind(), CredentialKind::OAuth);
    assert_eq!(resolved.origin, Origin::Env(ENV_AUTH_TOKEN));

    fs::remove_dir_all(&root).ok();
}

#[test]
fn blank_api_key_does_not_shadow_the_token() {
    // A leftover `export ANTHROPIC_API_KEY=` must not win the slot, or
    // the working token below it is never reached.
    let root = scratch("blank");
    let vars = env(&[
        (ENV_API_KEY, "   "),
        (ENV_AUTH_TOKEN, SECRET),
        (ENV_XDG_CONFIG_HOME, root.to_str().unwrap()),
    ]);

    let resolved = credentials::resolve(&vars).expect("resolve");
    assert_eq!(resolved.credential.kind(), CredentialKind::OAuth);
    assert_eq!(resolved.origin, Origin::Env(ENV_AUTH_TOKEN));

    fs::remove_dir_all(&root).ok();
}

#[test]
fn surrounding_whitespace_is_not_part_of_the_secret() {
    let vars = env(&[(ENV_API_KEY, "  sk-ant-padded\n")]);
    let resolved = credentials::resolve(&vars).expect("resolve");
    match resolved.credential {
        Credential::ApiKey(secret) => assert_eq!(secret.expose(), "sk-ant-padded"),
        other => panic!("expected an API key, got {other:?}"),
    }
}

#[test]
fn file_is_used_when_the_environment_is_silent() {
    let root = scratch("file");
    let path = write_config(&root, &format!(r#"{{"api_key": "{SECRET}"}}"#), 0o600);

    let vars = env(&[(ENV_XDG_CONFIG_HOME, root.to_str().unwrap())]);
    let resolved = credentials::resolve(&vars).expect("resolve");

    assert_eq!(resolved.credential.kind(), CredentialKind::ApiKey);
    assert_eq!(resolved.origin, Origin::File(path));

    fs::remove_dir_all(&root).ok();
}

#[test]
fn file_oauth_token_keeps_its_kind() {
    let root = scratch("file-oauth");
    write_config(&root, &format!(r#"{{"oauth_token": "{SECRET}"}}"#), 0o600);

    let vars = env(&[(ENV_XDG_CONFIG_HOME, root.to_str().unwrap())]);
    let resolved = credentials::resolve(&vars).expect("resolve");

    assert_eq!(resolved.credential.kind(), CredentialKind::OAuth);

    fs::remove_dir_all(&root).ok();
}

#[test]
fn config_falls_back_to_home_dot_config() {
    let root = scratch("home");
    let path = write_config(
        &root.join(".config"),
        &format!(r#"{{"api_key": "{SECRET}"}}"#),
        0o600,
    );

    let vars = env(&[(ENV_HOME, root.to_str().unwrap())]);
    assert_eq!(credentials::config_path(&vars).as_deref(), Some(&*path));

    let resolved = credentials::resolve(&vars).expect("resolve");
    assert_eq!(resolved.origin, Origin::File(path));

    fs::remove_dir_all(&root).ok();
}

#[cfg(unix)]
#[test]
fn a_file_others_can_read_is_refused() {
    // Group-readable, world-readable, and world-writable each have to be
    // refused on their own: it is the presence of any bit outside the
    // owner's that matters, not the exact mode.
    for mode in [0o640, 0o604, 0o666, 0o644] {
        let root = scratch("perm");
        let path = write_config(&root, &format!(r#"{{"api_key": "{SECRET}"}}"#), mode);
        let vars = env(&[(ENV_XDG_CONFIG_HOME, root.to_str().unwrap())]);

        let err = credentials::resolve(&vars).expect_err("must refuse");
        match &err {
            CredentialError::InsecurePermissions {
                path: reported,
                mode: reported_mode,
            } => {
                assert_eq!(reported, &path);
                assert_eq!(*reported_mode, mode, "reports the mode it found");
            }
            other => panic!("expected InsecurePermissions for {mode:03o}, got {other:?}"),
        }

        // The message has to say what to do about it, and must not carry
        // the secret it declined to use.
        let shown = err.to_string();
        assert!(shown.contains("chmod 600"), "unhelpful message: {shown}");
        assert!(!shown.contains(SECRET), "the error leaked the secret");

        fs::remove_dir_all(&root).ok();
    }
}

#[cfg(unix)]
#[test]
fn a_wide_open_file_is_an_error_not_a_miss() {
    // Refusing must not degrade into "no credential found": the user
    // pointed at that file, and silence would leave them guessing.
    let root = scratch("perm-miss");
    write_config(&root, &format!(r#"{{"api_key": "{SECRET}"}}"#), 0o644);
    let vars = env(&[(ENV_XDG_CONFIG_HOME, root.to_str().unwrap())]);

    assert!(matches!(
        credentials::resolve(&vars),
        Err(CredentialError::InsecurePermissions { .. })
    ));

    fs::remove_dir_all(&root).ok();
}

#[test]
fn missing_everywhere_names_the_file_it_looked_for() {
    let root = scratch("missing");
    let vars = env(&[(ENV_XDG_CONFIG_HOME, root.to_str().unwrap())]);

    match credentials::resolve(&vars) {
        Err(CredentialError::Missing { looked_in }) => assert_eq!(
            looked_in.as_deref(),
            Some(&*root.join(CONFIG_DIR).join(CONFIG_FILE))
        ),
        other => panic!("expected Missing, got {other:?}"),
    }

    fs::remove_dir_all(&root).ok();
}

#[test]
fn an_environment_without_a_home_still_answers() {
    let vars = env(&[]);
    assert_eq!(credentials::config_path(&vars), None);
    assert!(matches!(
        credentials::resolve(&vars),
        Err(CredentialError::Missing { looked_in: None })
    ));
}

#[test]
fn a_file_that_is_not_json_is_reported_as_such() {
    let root = scratch("malformed");
    write_config(&root, "this is not json", 0o600);
    let vars = env(&[(ENV_XDG_CONFIG_HOME, root.to_str().unwrap())]);

    assert!(matches!(
        credentials::resolve(&vars),
        Err(CredentialError::Malformed { .. })
    ));

    fs::remove_dir_all(&root).ok();
}

#[test]
fn a_file_naming_no_credential_is_reported_as_such() {
    let root = scratch("empty");
    write_config(&root, r#"{"model": "claude-opus-4-8"}"#, 0o600);
    let vars = env(&[(ENV_XDG_CONFIG_HOME, root.to_str().unwrap())]);

    assert!(matches!(
        credentials::resolve(&vars),
        Err(CredentialError::Empty { .. })
    ));

    fs::remove_dir_all(&root).ok();
}

#[test]
fn a_credential_that_is_not_a_string_is_named_as_the_mistake() {
    // Skipping it and then reporting "no credential here" would send the
    // user looking for a missing field that is right in front of them.
    let root = scratch("wrong-type");
    write_config(&root, r#"{"api_key": 12345}"#, 0o600);
    let vars = env(&[(ENV_XDG_CONFIG_HOME, root.to_str().unwrap())]);

    match credentials::resolve(&vars) {
        Err(CredentialError::Malformed { reason, .. }) => {
            assert!(reason.contains("api_key"), "unhelpful reason: {reason}");
        }
        other => panic!("expected Malformed, got {other:?}"),
    }

    fs::remove_dir_all(&root).ok();
}

#[test]
fn a_file_that_is_not_an_object_is_reported_as_such() {
    let root = scratch("not-object");
    write_config(&root, r#"["sk-ant-nope"]"#, 0o600);
    let vars = env(&[(ENV_XDG_CONFIG_HOME, root.to_str().unwrap())]);

    assert!(matches!(
        credentials::resolve(&vars),
        Err(CredentialError::Malformed { .. })
    ));

    fs::remove_dir_all(&root).ok();
}

#[test]
fn unknown_fields_do_not_stop_the_agent() {
    // Forward compatibility: a settings key added later must not make an
    // older build refuse the file it lives in.
    let root = scratch("extra");
    write_config(
        &root,
        &format!(r#"{{"api_key": "{SECRET}", "future_setting": 3}}"#),
        0o600,
    );
    let vars = env(&[(ENV_XDG_CONFIG_HOME, root.to_str().unwrap())]);

    assert!(credentials::resolve(&vars).is_ok());

    fs::remove_dir_all(&root).ok();
}

#[test]
fn the_secret_never_appears_in_debug_or_display() {
    let key = Credential::api_key(SECRET);
    let token = Credential::oauth(SECRET);
    let secret = Secret::new(SECRET);

    let renderings = [
        format!("{key:?}"),
        format!("{token:?}"),
        format!("{secret:?}"),
        format!("{secret}"),
        format!("{:?}", credentials::Resolved {
            credential: Credential::oauth(SECRET),
            origin: Origin::Env(ENV_AUTH_TOKEN),
        }),
        format!("{:?}", vec![Credential::api_key(SECRET)]),
    ];

    for shown in &renderings {
        assert!(!shown.contains(SECRET), "leaked: {shown}");
        assert!(shown.contains("redacted"), "not obviously redacted: {shown}");
    }

    // The kind is what may be shown instead, and it must still be there.
    assert_eq!(key.kind().to_string(), "API key");
    assert_eq!(token.kind().to_string(), "OAuth token");
}

#[test]
fn errors_never_carry_the_secret() {
    let root = scratch("leak");
    let path = write_config(&root, &format!(r#"{{"api_key": "{SECRET}"}}"#), 0o600);

    let shown = [
        CredentialError::Missing {
            looked_in: Some(path.clone()),
        },
        CredentialError::InsecurePermissions {
            path: path.clone(),
            mode: 0o644,
        },
        CredentialError::Unreadable {
            path: path.clone(),
            reason: "denied".to_string(),
        },
        CredentialError::Malformed {
            path: path.clone(),
            reason: "expected value at line 1 column 1".to_string(),
        },
        CredentialError::Empty { path: path.clone() },
    ];

    for err in &shown {
        assert!(!err.to_string().contains(SECRET), "leaked: {err}");
        assert!(!format!("{err:?}").contains(SECRET), "leaked: {err:?}");
    }

    fs::remove_dir_all(&root).ok();
}

#[test]
fn an_api_key_goes_in_the_api_key_header() {
    let headers = Credential::api_key(SECRET).auth_headers();
    assert_eq!(headers, vec![(HEADER_API_KEY, SECRET.to_string())]);
}

#[test]
fn an_oauth_token_is_a_bearer_and_brings_its_beta_flag() {
    // The bearer header alone is not enough — without the beta flag the
    // messages endpoint rejects an otherwise valid token, so the two
    // travel together or not at all.
    let headers = Credential::oauth(SECRET).auth_headers();
    assert_eq!(
        headers,
        vec![
            (HEADER_AUTHORIZATION, format!("Bearer {SECRET}")),
            (HEADER_ANTHROPIC_BETA, OAUTH_BETA.to_string()),
        ]
    );
    assert!(headers.iter().all(|(name, _)| *name != HEADER_API_KEY));
}

#[test]
fn load_file_can_be_pointed_at_a_path_directly() {
    let root = scratch("direct");
    let path = write_config(&root, &format!(r#"{{"api_key": "{SECRET}"}}"#), 0o600);

    let credential = credentials::load_file(&path).expect("load");
    assert_eq!(credential.kind(), CredentialKind::ApiKey);

    fs::remove_dir_all(&root).ok();
}
