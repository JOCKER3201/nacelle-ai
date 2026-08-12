//! The four layers, one at a time, and the ordering between them.
//!
//! Nothing here touches the network — not even a local model. Every
//! reviewer is written down in advance, because what is being tested is
//! not whether a model has good judgement; it is that the layers which
//! do not depend on judgement run first, and that the one which does can
//! only ever remove more.
//!
//! The secrets below are invented. They have the SHAPE of the real
//! thing, which is what the scanner matches on, and no value: nothing in
//! this repository is a credential, including in a test.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use nacelle_ai::backend::{Backend, EventSink};
use nacelle_ai::redact::deny::{self, Denylist, Reason};
use nacelle_ai::redact::scan::{scan, Kind};
use nacelle_ai::redact::{
    Disclosure, Gathering, LocalReviewer, NoReview, Removal, Review, Reviewer, Why,
};
use nacelle_ai::{BackendError, Request, ToolError};

fn scratch(tag: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "nacelle-ai-redact-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// A home directory with something worth refusing in each of the places
/// the denylist knows about.
fn home(tag: &str) -> PathBuf {
    let home = scratch(tag);
    for (path, contents) in [
        (".ssh/id_ed25519", "PRIVATE"),
        (".ssh/config", "Host example\n"),
        (".gnupg/secring.gpg", "PRIVATE"),
        (".aws/credentials", "[default]\n"),
        (".config/gh/hosts.yml", "github.com:\n"),
        (".config/anthropic/settings.json", "{}\n"),
        (".config/nacelle-ai/credentials.json", "{}\n"),
        (".local/share/keyrings/login.keyring", "keyring\n"),
        (".password-store/mail.gpg", "PRIVATE"),
        (".mozilla/firefox/profile/cookies.sqlite", "cookies"),
        (".bash_history", "export TOKEN=x\n"),
        // The same thing under the name zsh gives it when HISTFILE is
        // set the short way — a history is a history whatever the shell
        // decided to call it.
        (".histfile", "export TOKEN=x\n"),
        ("work/.env", "API_TOKEN=x\n"),
        ("work/.env.production", "API_TOKEN=x\n"),
        ("work/server.pem", "cert"),
        ("work/notes.txt", "ordinary notes\n"),
    ] {
        let path = home.join(path);
        fs::create_dir_all(path.parent().expect("a parent")).expect("directory");
        fs::write(&path, contents).expect("file");
    }
    home
}

// ---- layer 1: never read it ----------------------------------------

/// One case per family on the list. Each is a real read attempt, and
/// each has to come back refused rather than with the contents.
#[test]
fn every_kind_of_secret_store_is_refused_before_it_is_opened() {
    let home = home("deny");
    let guard = Denylist::new(Some(&home));

    for relative in [
        ".ssh/id_ed25519",
        ".ssh/config",
        ".gnupg/secring.gpg",
        ".aws/credentials",
        ".config/gh/hosts.yml",
        ".config/anthropic/settings.json",
        ".config/nacelle-ai/credentials.json",
        ".local/share/keyrings/login.keyring",
        ".password-store/mail.gpg",
        ".mozilla/firefox/profile/cookies.sqlite",
        ".bash_history",
        ".histfile",
        "work/.env",
        "work/.env.production",
        "work/server.pem",
    ] {
        let err = guard
            .read_to_string(&home.join(relative))
            .expect_err("must be refused");
        assert!(
            matches!(err, ToolError::Denied { .. }),
            "{relative} gave {err:?}"
        );
    }

    // And the control: an ordinary file in the same tree is readable,
    // because a denylist that refuses everything protects nothing.
    assert_eq!(
        guard
            .read_to_string(&home.join("work/notes.txt"))
            .expect("an ordinary file must still be readable"),
        "ordinary notes\n"
    );
}

/// A refusal is an answer the user is given, not a hole in the output.
/// It names the file and the rule, and it says the rule cannot be
/// argued with — because the user may be asking for a perfectly good
/// reason and needs to know to open the file themselves.
#[test]
fn a_refusal_says_which_file_and_why() {
    let home = home("reported");
    let guard = Denylist::new(Some(&home));

    let err = guard
        .read_to_string(&home.join(".ssh/id_ed25519"))
        .expect_err("must be refused");
    let said = err.to_string();

    assert!(said.contains("id_ed25519"), "{said}");
    assert!(said.contains("credential store"), "{said}");
    assert!(said.contains("open it yourself"), "{said}");
    assert!(
        !said.contains("PRIVATE"),
        "a refusal must not carry what it refused to read: {said}"
    );
}

/// The check is on the canonical path, so a link is the file it points
/// at. This is the case a check on the name alone gets wrong, and it is
/// also the case an attacker — or a careless build script — produces.
#[cfg(unix)]
#[test]
fn a_symlink_into_a_denied_directory_is_the_denied_file() {
    let home = home("symlink");
    let guard = Denylist::new(Some(&home));
    let link = home.join("work").join("innocent.txt");
    std::os::unix::fs::symlink(home.join(".ssh/id_ed25519"), &link).expect("symlink");

    let err = guard.read_to_string(&link).expect_err("must be refused");
    assert!(matches!(err, ToolError::Denied { .. }), "{err:?}");

    // And `..` cannot walk out of an allowed name into a denied
    // directory either.
    let sideways = home.join("work").join("..").join(".ssh").join("config");
    assert!(
        matches!(
            guard.read_to_string(&sideways),
            Err(ToolError::Denied { .. })
        ),
        "a path through .. must resolve before it is judged"
    );
}

/// The extension and directory rules can both be avoided by naming a key
/// something innocent and putting it somewhere ordinary. The content
/// sniff is what is left, and it runs before a single byte is returned.
#[test]
fn a_private_key_under_an_innocent_name_is_refused_by_its_first_line() {
    let dir = scratch("armour");
    let guard = Denylist::new(None);
    let path = dir.join("meeting-notes.txt");
    fs::write(
        &path,
        "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----\n",
    )
    .expect("write");

    let err = guard.read_to_string(&path).expect_err("must be refused");
    let said = err.to_string();
    assert!(matches!(err, ToolError::Denied { .. }), "{err:?}");
    assert!(said.contains("PEM private-key header"), "{said}");
    assert!(!said.contains("b3BlbnNzaC"), "{said}");

    // A certificate is public by construction and is not refused: a rule
    // that fires on things nobody minds is one the user learns to
    // ignore.
    let cert = dir.join("server-cert.txt");
    fs::write(&cert, "-----BEGIN CERTIFICATE-----\nMIIB\n").expect("write");
    assert!(guard.read_to_string(&cert).is_ok());

    // The sniff reads a fixed number of BYTES, and the file it is
    // sniffing is text. A file whose thousandth byte lands in the middle
    // of a character must come back whole rather than as "not UTF-8".
    let prose = dir.join("długi-tekst.txt");
    let text = "zażółć gęślą jaźń — ".repeat(200);
    fs::write(&prose, &text).expect("write");
    assert!(text.len() > 1024, "the file has to cross the sniff boundary");
    assert_eq!(guard.read_to_string(&prose).expect("readable"), text);
}

/// The name rules do not need a home directory, so an environment that
/// names none still refuses key material — and the list only ever grows.
#[test]
fn the_list_cannot_be_shortened_only_lengthened() {
    let dir = scratch("floor");
    let guard = Denylist::new(None);
    fs::write(dir.join("id_rsa"), "PRIVATE").expect("write");
    fs::write(dir.join(".bash_history"), "history").expect("write");
    fs::write(dir.join("plain.txt"), "plain").expect("write");

    assert!(guard.read_to_string(&dir.join("id_rsa")).is_err());
    assert!(guard.read_to_string(&dir.join(".bash_history")).is_err());
    assert!(guard.read_to_string(&dir.join("plain.txt")).is_ok());

    // `also` is the only mutation there is, and it takes things away
    // from what may be read rather than giving them back.
    let stricter = guard.clone().also(&dir, Reason::CredentialStore);
    assert!(
        stricter.read_to_string(&dir.join("plain.txt")).is_err(),
        "a directory added to the list must be refused"
    );
    assert!(
        stricter.directories().len() > guard.directories().len(),
        "the list must have grown"
    );
}

/// The list, walked. Not a sample of it — every entry, each with a file
/// inside it, each read attempted.
///
/// A denylist tested by example is a denylist whose next entry arrives
/// untested, and an entry that never matched anything looks exactly like
/// one that does. So this test has no list of its own: it reads the
/// program's, and an entry added tomorrow is covered tomorrow.
#[test]
fn every_directory_on_the_list_refuses_a_read_from_inside_it() {
    let home = scratch("bylist");
    let mut probes: Vec<PathBuf> = Vec::new();
    for (relative, _) in deny::HOME_DIRS {
        probes.push(home.join(relative).join("probe.txt"));
    }
    for (relative, _) in deny::CONFIG_DIRS {
        probes.push(home.join(".config").join(relative).join("probe.txt"));
    }
    for (relative, _) in deny::DATA_DIRS {
        probes.push(home.join(".local/share").join(relative).join("probe.txt"));
    }
    assert!(probes.len() > 20, "the list has gone missing: {probes:?}");

    for probe in &probes {
        fs::create_dir_all(probe.parent().expect("a parent")).expect("directory");
        fs::write(probe, "ordinary text\n").expect("file");
    }

    let guard = Denylist::new(Some(&home));
    for probe in &probes {
        let err = guard.read_to_string(probe).expect_err("must be refused");
        assert!(matches!(err, ToolError::Denied { .. }), "{probe:?}: {err:?}");
    }
}

/// The same for the rules that are a name or an extension rather than a
/// directory, and for the shapes those rules cover without naming.
#[test]
fn every_name_and_extension_on_the_list_refuses_a_read() {
    let dir = scratch("bynames");
    let guard = Denylist::new(None);

    let mut names: Vec<String> = deny::HISTORY_NAMES.iter().map(|n| (*n).to_string()).collect();
    names.extend(
        deny::KEY_EXTENSIONS
            .iter()
            .map(|ext| format!("material.{ext}")),
    );
    // The rules with no list behind them: the suffixes every shell's
    // history follows, the environment files, and the key names
    // `ssh-keygen` writes.
    names.extend(
        [
            ".bash_history",
            ".zsh_history",
            ".psql_history",
            ".node_repl_history",
            "fish_history",
            ".octave_hist",
            ".env",
            ".env.production",
            "staging.env",
            "id_rsa",
            "id_ed25519.pub",
        ]
        .iter()
        .map(|n| (*n).to_string()),
    );

    for name in &names {
        let path = dir.join(name);
        fs::write(&path, "whatever is in it\n").expect("file");
        let err = guard.read_to_string(&path).expect_err("must be refused");
        assert!(matches!(err, ToolError::Denied { .. }), "{name}: {err:?}");
    }

    // And the control, in the same directory: the rules are narrow
    // enough that ordinary files are still readable.
    for name in ["notes.txt", "history.md", "identity.json", "environment.rs"] {
        let path = dir.join(name);
        fs::write(&path, "ordinary\n").expect("file");
        assert!(guard.read_to_string(&path).is_ok(), "{name} was refused");
    }
}

/// The list read against the machines it actually runs on. Every path
/// below is where a real program keeps a real credential — plain text in
/// most cases — and every one of them is in the home directory of a
/// developer's desktop, which is the machine this agent runs on.
#[test]
fn the_credential_stores_a_real_home_directory_has_are_all_refused() {
    let home = scratch("real");
    let stores = [
        // git's own plain-text store, and the one it keeps under XDG.
        ".git-credentials",
        ".config/git/credentials",
        // Language and package tooling, each of which writes a token to
        // a file with no extension worth matching.
        ".npmrc",
        ".pypirc",
        ".cargo/credentials.toml",
        ".gem/credentials",
        // Databases: both of these are passwords in plain text by
        // design, and both are read by the client without being asked.
        ".pgpass",
        ".my.cnf",
        // Certificate and key databases the browsers and NSS share.
        ".pki/nssdb/key4.db",
        ".gnome2/keyrings/login.keyring",
        // Container registries and infrastructure tooling.
        ".config/containers/auth.json",
        ".terraform.d/credentials.tfrc.json",
        ".subversion/auth/svn.simple/whatever",
        ".config/rclone/rclone.conf",
        // Password managers with a file rather than a database.
        ".config/Bitwarden/data.json",
        ".config/sops/age/keys.txt",
        // This agent's cousin. Its token is as much a credential as the
        // one this program keeps in its own configuration directory.
        ".claude/.credentials.json",
        // Histories no suffix rule covers: R writes one, the mongo
        // shell writes one, and the two shells that keep theirs in a
        // database write theirs under the data directory.
        ".Rhistory",
        ".dbshell",
        ".local/share/atuin/history.db",
        ".local/share/nushell/history.txt",
    ];

    for relative in stores {
        let path = home.join(relative);
        fs::create_dir_all(path.parent().expect("a parent")).expect("directory");
        fs::write(&path, "a credential lives here\n").expect("file");
    }

    let guard = Denylist::new(Some(&home));
    for relative in stores {
        let err = guard
            .read_to_string(&home.join(relative))
            .expect_err("must be refused");
        assert!(
            matches!(err, ToolError::Denied { .. }),
            "{relative} was read: {err:?}"
        );
    }
}

/// The denylist is only worth anything if the read paths actually go
/// through it. This is the tool a model calls to read a layaut, and the
/// file it names passes confinement — it is inside the data directory,
/// and it is not a symlink. What refuses it is the guard, on content.
#[test]
fn the_tools_read_through_the_guard_and_not_around_it() {
    let root = scratch("toolbox");
    let config = root.join("config").join("nacelle");
    let data = root.join("data").join("nacelle");
    fs::create_dir_all(config.join("..")).expect("config");
    fs::create_dir_all(data.join("layauts")).expect("layauts");
    fs::write(
        data.join("layauts").join("innocent.layaut"),
        "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkq\n-----END PRIVATE KEY-----\n",
    )
    .expect("write");

    let tools = nacelle_ai::Toolbox::new(nacelle_ai::DesktopDirs::new(Some(config), Some(data)));
    let err = tools
        .run(
            nacelle_ai::tools::TOOL_READ_LAYAUT,
            &serde_json::json!({ "name": "innocent" }),
        )
        .expect_err("must be refused");

    assert!(matches!(err, ToolError::Denied { .. }), "{err:?}");
    assert!(
        !err.to_string().contains("MIIEvQIBADANBgkq"),
        "the refusal must not carry what it refused"
    );
}

/// And the payload builder can do the read itself, so a caller cannot
/// assemble one out of a file that layer 1 would have refused.
#[test]
fn a_payload_cannot_be_built_out_of_a_file_the_denylist_refuses() {
    let home = home("payload");
    let guard = Denylist::new(Some(&home));

    let err = Gathering::new()
        .read_file(&guard, home.join(".ssh/id_ed25519"))
        .expect_err("must be refused");
    assert!(matches!(err, ToolError::Denied { .. }), "{err:?}");

    let out = Gathering::new()
        .read_file(&guard, home.join("work/notes.txt"))
        .expect("an ordinary file goes in")
        .unreviewed();
    assert_eq!(out.sources().len(), 1);
    assert!(out.payload().contains("ordinary notes"));
}

// ---- layer 2: never send it ----------------------------------------

/// Every shape the design names, in one payload, from several
/// directions. The assertion that matters is the same for all of them:
/// the value is not in the output.
#[test]
fn every_credential_shape_is_cut_out_of_an_outgoing_payload() {
    let cases: &[(&str, &str)] = &[
        ("sk-ant-api03-EXAMPLE-NOT-A-REAL-KEY-0000000000", "Anthropic"),
        ("sk-proj-EXAMPLE-NOT-A-REAL-KEY-0000000000", "OpenAI-style"),
        ("ghp_EXAMPLE_NOT_A_REAL_TOKEN_00000000", "GitHub"),
        ("gho_EXAMPLE_NOT_A_REAL_TOKEN_00000000", "GitHub"),
        ("AKIAEXAMPLENOTREAL00", "AWS"),
        ("xoxb-EXAMPLE-NOT-A-REAL-TOKEN-00000000", "Slack"),
        ("AIzaEXAMPLE-NOT-A-REAL-KEY-00000000000", "Google"),
        (
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJleGFtcGxlIn0.EXAMPLE-NOT-A-REAL-SIGNATURE",
            "JSON web token",
        ),
    ];

    for (secret, expected) in cases {
        let text = format!("Here is the value the log printed: {secret} — please look at it.");
        let out = scan(&text);
        assert!(
            !out.text.contains(secret),
            "{expected}: the value survived: {}",
            out.text
        );
        assert!(
            out.text.contains("[[redacted:"),
            "{expected}: nothing was marked: {}",
            out.text
        );
        assert!(
            out.text.contains(expected),
            "the marker must say what was removed: {}",
            out.text
        );
        assert_eq!(out.findings.len(), 1, "{expected}: {:?}", out.findings);
    }
}

/// The shapes that are not a bare word: armour blocks, header lines,
/// connection strings, and a value whose own name says what it is.
#[test]
fn armour_headers_connection_strings_and_labelled_values_go_too() {
    let text = "\
-----BEGIN RSA PRIVATE KEY-----
EXAMPLE-NOT-A-REAL-KEY-0000000000000000000
-----END RSA PRIVATE KEY-----
Authorization: Bearer EXAMPLE-NOT-A-REAL-TOKEN-000000
DATABASE_URL=postgres://appuser:example-not-a-real-password@db.internal:5432/app
API_TOKEN = EXAMPLE-NOT-A-REAL-TOKEN-0000
";

    let out = scan(text);
    for gone in [
        "EXAMPLE-NOT-A-REAL-KEY-0000000000000000000",
        "EXAMPLE-NOT-A-REAL-TOKEN-000000",
        "example-not-a-real-password",
        "EXAMPLE-NOT-A-REAL-TOKEN-0000",
    ] {
        assert!(!out.text.contains(gone), "{gone} survived:\n{}", out.text);
    }

    // What is kept is what makes the rest intelligible: the header's
    // name, the scheme, the user and the host.
    assert!(out.text.contains("Authorization:"), "{}", out.text);
    assert!(out.text.contains("postgres://appuser:"), "{}", out.text);
    assert!(out.text.contains("@db.internal:5432/app"), "{}", out.text);

    let kinds: Vec<&Kind> = out.findings.iter().map(|f| &f.kind).collect();
    assert!(kinds.contains(&&Kind::PrivateKey), "{kinds:?}");
    assert!(kinds.contains(&&Kind::ConnectionPassword), "{kinds:?}");
}

/// The other half of the bargain. A scanner that redacts ordinary prose
/// and paths teaches the user that markers are noise, and a marker the
/// user ignores is worse than no marker.
#[test]
fn ordinary_text_and_paths_are_left_alone() {
    let text = "\
The layaut file lives at /home/michael/.local/share/nacelle-desktop/layauts/wide.layaut
and the theme it names is crimson. The file is read by nacelle-desktop
when it starts rather than watched, and createDefaultConfigurationBuilder
is where that happens. Nothing here is a credential.
";
    let out = scan(text);
    assert_eq!(out.text, text, "nothing in this should have been touched");
    assert!(out.is_clean(), "{:?}", out.findings);
}

/// A digest is not left alone any more, and this is the test that says
/// so out loud rather than a behaviour a reader has to discover.
///
/// The rule used to demand three character classes, which meant it never
/// looked at hexadecimal — two classes — and so never looked at
/// `xxd -p` output, at base32, at a Twilio auth token (thirty-two
/// lower-case hex) or at a GitHub token minted before 2021 (forty). The
/// adversary measured all four going out whole.
///
/// The price of closing that is on this line: **a forty-character git
/// revision and a forty-character GitHub token are the same forty
/// characters**, and nothing can tell them apart. So a commit hash in a
/// payload now gets a marker, and the far model is told a value was
/// withheld rather than being handed one that might have been a token.
/// The user pays a sentence; the alternative was paying a credential.
#[test]
fn a_digest_is_cut_because_it_cannot_be_told_from_a_token() {
    let digest = "ba1ddd6f2c0a1e4d9b8c7a6f5e4d3c2b1a0f9e8d";
    let out = scan(&format!("The commit {digest} is the one that broke it."));
    assert!(!out.text.contains(digest), "{}", out.text);
    assert_eq!(out.findings.len(), 1, "{:?}", out.findings);
    assert_eq!(out.findings[0].kind, Kind::HighEntropy);

    // A short revision is still a short revision: the entropy rule will
    // not speak below its minimum length, so `git log --oneline` and
    // every other everyday abbreviation is untouched.
    let short = scan("The commit ba1ddd6 is the one that broke it.");
    assert!(short.is_clean(), "{:?}", short.findings);
}

/// The catch-all, for a credential nobody gave a recognisable prefix.
#[test]
fn a_long_high_entropy_run_is_cut_even_without_a_known_prefix() {
    let secret = "Zk9-vQ2xR7pLmXe4RdYuAzC1oGjB6nHs8tKq2wVbNf";
    let out = scan(&format!("the value is {secret} and that is all"));
    assert!(!out.text.contains(secret), "{}", out.text);
    assert_eq!(out.findings.len(), 1, "{:?}", out.findings);
    assert_eq!(out.findings[0].kind, Kind::HighEntropy);
}

/// The case `docs/supervisor.md` names in as many words: "a token
/// embedded in a URL query string". It is not at the start of anything —
/// `=` holds a run of token characters together, so the key sits in the
/// middle of one — and a rule that only looked at where a run BEGINS
/// would send it.
///
/// What survives matters as much as what goes: the host and the path are
/// how the far model works out what the request was, and neither grants
/// anything.
#[test]
fn a_token_in_the_middle_of_a_url_is_found_and_named() {
    let url = "GET https://api.example.com/v1/models?key=sk-ant-api03-EXAMPLE-NOT-REAL-000 failed";
    let out = scan(url);

    assert!(
        !out.text.contains("sk-ant-api03-EXAMPLE-NOT-REAL-000"),
        "the key survived: {}",
        out.text
    );
    assert!(
        out.text.contains("https://api.example.com/v1/models?key="),
        "the request must stay intelligible: {}",
        out.text
    );
    assert_eq!(out.findings.len(), 1, "{:?}", out.findings);
    assert_eq!(
        out.findings[0].kind,
        Kind::ApiKey("an Anthropic API key"),
        "the marker has to name what went, not call it a long string"
    );

    // The same key in the other two places a URL hides one: a path
    // segment, and a query parameter with more parameters after it.
    for url in [
        "https://api.example.com/v1/ghp_EXAMPLE_NOT_A_REAL_TOKEN_00000000/models",
        "https://api.example.com/v1/models?page=2&key=ghp_EXAMPLE_NOT_A_REAL_TOKEN_00000000&limit=10",
    ] {
        let out = scan(url);
        assert!(
            !out.text.contains("ghp_EXAMPLE_NOT_A_REAL_TOKEN_00000000"),
            "the key survived: {}",
            out.text
        );
        assert!(
            out.text.contains("https://api.example.com/v1/"),
            "the request must stay intelligible: {}",
            out.text
        );
        assert_eq!(
            out.findings[0].kind,
            Kind::ApiKey("a GitHub token"),
            "{:?}",
            out.findings
        );
    }
}

/// The rest of that hole, which the URL case hid rather than closed.
///
/// `=` and `/` are not the only characters a run is held together by.
/// A key pasted after a dotted name, after a hyphenated one, or after
/// the kind of prefix an environment variable is given sits in the
/// middle of a run just as surely as one in a query string — and every
/// line below went out WHOLE, measured on the scanner as it stood:
///
/// ```text
/// AKIAIOSFODNN7EXAMPLE                              cut
/// amazon.web.services.account.AKIAIOSFODNN7EXAMPLE  sent
/// profile-AKIAIOSFODNN7EXAMPLE                      sent
/// aws_AKIAIOSFODNN7EXAMPLE                          sent
/// url=AKIAIOSFODNN7EXAMPLE                          cut
/// ```
///
/// Which family it was matters as much as that it went: a marker that
/// says "a long high-entropy string" where it could have said "an AWS
/// access key id" is what makes the far model guess.
#[test]
fn a_prefix_behind_a_word_joiner_is_still_a_prefix() {
    let cases: &[(&str, &str, &str, Kind)] = &[
        // The four that were measured, and the two that already worked
        // beside them — a widening that quietly dropped one of those
        // would trade one hole for another.
        (
            "amazon.web.services.account.AKIAIOSFODNN7EXAMPLE",
            "AKIAIOSFODNN7EXAMPLE",
            "amazon.web.services.account.",
            Kind::ApiKey("an AWS access key id"),
        ),
        (
            "profile-AKIAIOSFODNN7EXAMPLE",
            "AKIAIOSFODNN7EXAMPLE",
            "profile-",
            Kind::ApiKey("an AWS access key id"),
        ),
        (
            "aws_AKIAIOSFODNN7EXAMPLE",
            "AKIAIOSFODNN7EXAMPLE",
            "aws_",
            Kind::ApiKey("an AWS access key id"),
        ),
        (
            "url=AKIAIOSFODNN7EXAMPLE",
            "AKIAIOSFODNN7EXAMPLE",
            "url=",
            Kind::ApiKey("an AWS access key id"),
        ),
        (
            "AKIAIOSFODNN7EXAMPLE",
            "AKIAIOSFODNN7EXAMPLE",
            "",
            Kind::ApiKey("an AWS access key id"),
        ),
        // And the same joiner behind the other families, because the
        // anchor is one rule and not one rule per provider.
        (
            "deploy.ghp_EXAMPLE_NOT_A_REAL_TOKEN_00000000",
            "ghp_EXAMPLE_NOT_A_REAL_TOKEN_00000000",
            "deploy.",
            Kind::ApiKey("a GitHub token"),
        ),
        (
            "env_ghp_EXAMPLE_NOT_A_REAL_TOKEN_00000000",
            "ghp_EXAMPLE_NOT_A_REAL_TOKEN_00000000",
            "env_",
            Kind::ApiKey("a GitHub token"),
        ),
        (
            "anthropic-sk-ant-api03-EXAMPLE-NOT-REAL-000",
            "sk-ant-api03-EXAMPLE-NOT-REAL-000",
            "anthropic-",
            Kind::ApiKey("an Anthropic API key"),
        ),
        (
            "google.AIzaEXAMPLE-NOT-A-REAL-KEY-00000000000",
            "AIzaEXAMPLE-NOT-A-REAL-KEY-00000000000",
            "google.",
            Kind::ApiKey("a Google API key"),
        ),
        (
            "slack_xoxb-EXAMPLE-NOT-A-REAL-TOKEN-00000000",
            "xoxb-EXAMPLE-NOT-A-REAL-TOKEN-00000000",
            "slack_",
            Kind::ApiKey("a Slack bot token"),
        ),
    ];

    for (line, secret, kept, expected) in cases {
        let out = scan(line);
        assert!(
            !out.text.contains(secret),
            "the value survived: {}",
            out.text
        );
        // What sits in front of the key is not the key. Cutting the
        // whole run would take the name that says which account, which
        // service or which environment this was, and that name is how
        // the user recognises the finding on the manifest.
        assert!(
            out.text.starts_with(kept),
            "the text in front of the key went with it: {}",
            out.text
        );
        assert_eq!(out.findings.len(), 1, "{line} gave {:?}", out.findings);
        assert_eq!(out.findings[0].kind, *expected, "{line}");
    }

    // And the alphabet entire, rather than a sample of it. These six are
    // every character that holds a run together — the scanner's `TOKEN`,
    // written out here because it is private — and a key behind any one
    // of them is a key. The hole this test exists for was two of the six
    // being handled and four not, which is a hole no example finds
    // unless the example happens to be one of the four.
    for joiner in ['-', '_', '.', '/', '+', '='] {
        let line = format!("account{joiner}AKIAIOSFODNN7EXAMPLE");
        let out = scan(&line);
        assert!(
            !out.text.contains("AKIAIOSFODNN7EXAMPLE"),
            "a key behind {joiner:?} survived: {}",
            out.text
        );
        assert_eq!(
            out.findings[0].kind,
            Kind::ApiKey("an AWS access key id"),
            "behind {joiner:?}: {:?}",
            out.findings
        );
    }
}

/// The bill for the test above, and it is the half that decides whether
/// this layer is worth having. A scanner that cuts a locale directory,
/// a hyphenated file name or a supplier's hostname out of a message is
/// one the user stops reading the markers of — and a marker nobody
/// reads is the same as no marker at all.
///
/// Every line below contains a provider prefix, spelled exactly, and
/// not one of them is a credential. The reasons they are not divide
/// into three, and the rule has to answer each:
///
/// * **Inside a word.** `whisk-` ends in `sk-` and is a kitchen
///   implement. This is the case the anchor was written for and the one
///   a wider anchor must not give away.
/// * **Behind a joiner, but too short to be distinctive.** `-`, `_` and
///   `.` are how compound names are built, so a three-character prefix
///   behind one of them is a syllable: `homepage-sk-translation-draft`
///   is a page, not an OpenAI key.
/// * **Long enough only because of the punctuation after it.** A path
///   and a hostname keep going through `/` and `.`, and no provider
///   puts either character in a key. `sk-SK/LC_MESSAGES/gtk30.mo` is
///   twenty-six characters; the key it looks like is five.
#[test]
fn the_wider_anchor_does_not_widen_what_is_cut() {
    for ordinary in [
        // Inside a word.
        "The recipe app lives in /home/michael/projects/whisk-and-fold/README.md today.",
        "A risk-assessment-workshop-2024 is booked for the Thursday after next.",
        // Behind a joiner, and a syllable rather than a prefix.
        "The page homepage-sk-translation-draft.html is still waiting for review.",
        "Our supplier is at www.sk-telecom.example.com and has been since March.",
        "Set config.sk-locale-override=true if the browser guesses wrong.",
        "The ticket NACELLE-1234-sk-review-notes-final is the one with the trace in it.",
        "Write to michael.sk-consulting@example.com rather than to the old address.",
        // Long enough only because a path or a host kept going.
        "The Slovak build reads /usr/share/locale/sk-SK/LC_MESSAGES/gtk30.mo at start-up.",
        // Ordinary identifiers that happen to start with what a rule
        // looks for: a region constant, a package, a module path, a
        // revision.
        "The bucket is in AWS_REGION_ASIA_PACIFIC_SOUTHEAST_1 and always has been.",
        "Pinned to scikit-learn-1.4.2-py3-none-any.whl because 1.5 broke the loader.",
        "It is nacelle_ai_core::redact::scan::credential_in that decides this.",
        // The full revision that used to be on this list is now cut, on
        // purpose — see `a_digest_is_cut_because_it_cannot_be_told_from_a_token`.
        // The abbreviated one a person actually types is not.
        "The commit ba1ddd6 is the one that broke it.",
        "Try @babel/plugin-transform-runtime-corejs3 instead of the older one.",
    ] {
        let out = scan(ordinary);
        assert!(
            out.is_clean(),
            "an ordinary line was redacted: {ordinary}\n  -> {}\n  {:?}",
            out.text,
            out.findings
        );
        assert_eq!(out.text, ordinary);
    }
}

/// A payload is as likely to be one line of JSON as it is to be a `.env`
/// file, and on one line the interesting name is rarely the first one.
/// The value keyed by `user` is not a secret; the one next to it is.
#[test]
fn every_labelled_pair_on_a_line_is_examined_not_only_the_first() {
    let line = r#"{"user":"michael","api_key":"NOT-A-REAL-VALUE-0000","region":"eu"}"#;
    let out = scan(line);

    assert!(
        !out.text.contains("NOT-A-REAL-VALUE-0000"),
        "the value survived: {}",
        out.text
    );
    // The shape of the object is untouched, so the far model can still
    // see what kind of thing this was.
    assert!(out.text.contains(r#""user":"michael""#), "{}", out.text);
    assert!(out.text.contains(r#""region":"eu""#), "{}", out.text);
    assert_eq!(out.findings.len(), 1, "{:?}", out.findings);
    // The fragment the rule matched on, not the name it was found in:
    // `Kind::Labelled` holds a `&'static str` so that a marker cannot
    // carry a byte of the payload. See the variant's own comment.
    assert_eq!(out.findings[0].kind, Kind::Labelled("api_key"));
}

/// Two secrets on one line, and the shape of the line is what would
/// hide the second one: a rule that walked on after the first pair it
/// understood would send everything to the right of it.
#[test]
fn a_second_secret_on_the_same_line_goes_too() {
    let line = r#"{"user":"michael","api_key":"NOT-A-REAL-VALUE-0000","token":"ALSO-NOT-REAL-1111"}"#;
    let out = scan(line);

    assert!(!out.text.contains("NOT-A-REAL-VALUE-0000"), "{}", out.text);
    assert!(!out.text.contains("ALSO-NOT-REAL-1111"), "{}", out.text);
    assert!(out.text.contains(r#""user":"michael""#), "{}", out.text);
    assert_eq!(out.findings.len(), 2, "{:?}", out.findings);
}

/// The same shape a shell writes: two assignments on one line, joined by
/// the separator that ends a command rather than by a comma.
#[test]
fn two_assignments_on_one_command_line_both_go() {
    let line = "export API_KEY=NOT-A-REAL-KEY-0000; export SECRET=ALSO-NOT-REAL-1111";
    let out = scan(line);

    assert!(!out.text.contains("NOT-A-REAL-KEY-0000"), "{}", out.text);
    assert!(!out.text.contains("ALSO-NOT-REAL-1111"), "{}", out.text);
    assert_eq!(out.findings.len(), 2, "{:?}", out.findings);
}

/// A name and its value do not have to share a line, and everything that
/// writes configuration puts them on two: pretty-printed JSON breaks
/// after the colon, a YAML mapping puts the value under the key, and a
/// program that wraps its own output does both. A scan that ended at the
/// newline read the name, agreed it was a secret, found nothing beside
/// it and sent the value on the line below untouched.
#[test]
fn a_value_on_the_line_below_its_name_is_still_the_value() {
    let json = "{\n  \"api_key\":\n    \"NOT-A-REAL-VALUE-000000\",\n  \"region\": \"eu\"\n}";
    let out = scan(json);
    assert!(
        !out.text.contains("NOT-A-REAL-VALUE-000000"),
        "the value survived the line break:\n{}",
        out.text
    );
    assert!(out.text.contains(r#""region": "eu""#), "{}", out.text);

    let yaml = "database:\n  password:\n    correct-horse-battery-staple\n  port: 5432\n";
    let out = scan(yaml);
    assert!(
        !out.text.contains("correct-horse-battery-staple"),
        "the value survived the line break:\n{}",
        out.text
    );
    assert!(out.text.contains("port: 5432"), "{}", out.text);

    // The header case is the same hole: `Authorization` is not a word
    // that appears in the names the labelled rule knows, so nothing else
    // would catch this.
    let folded = "GET /v1/models\nAuthorization:\n  NOT-A-REAL-TOKEN-000000\nAccept: text/event-stream\n";
    let out = scan(folded);
    assert!(
        !out.text.contains("NOT-A-REAL-TOKEN-000000"),
        "the header value survived the fold:\n{}",
        out.text
    );
    assert!(out.text.contains("Accept: text/event-stream"), "{}", out.text);
}

/// And the other half of that bargain: a name with a mapping under it is
/// a structure, not a value. Swallowing the first line of the mapping
/// would take a user name that is not a secret and leave the password,
/// which is — and the password is caught by the rule for its own line.
#[test]
fn a_name_with_a_mapping_under_it_does_not_swallow_the_first_field() {
    let text = "credentials:\n  user: michael\n  password: hunter2-not-real\n";
    let out = scan(text);

    assert!(out.text.contains("user: michael"), "{}", out.text);
    assert!(!out.text.contains("hunter2-not-real"), "{}", out.text);

    // Nor does a blank line, a comment, or a document that simply has
    // colons at the ends of its lines. The rule reaches one line, and
    // only for something that could be a value.
    let gap = "password:\n\nplease call me back about it\n";
    assert!(scan(gap).text.contains("please call me back about it"));

    let commented = "api_key:\n# set this in the deploy script\nAPI_HOST=example.com\n";
    assert!(scan(commented)
        .text
        .contains("set this in the deploy script"));

    let prose = "What layer 2 does:\nIt scans every string in the request.\n\nWhy it is second:\nBecause a model's judgement is a probability.\n";
    let out = scan(prose);
    assert_eq!(out.text, prose, "{:?}", out.findings);
}

/// A hard wrap in the middle of a value leaves a tail no rule can
/// recognise on its own: it is shorter than the entropy rule's minimum,
/// it is often of only two character classes, and it has no prefix to
/// name it. What says what it is, is the line above.
#[test]
fn the_tail_of_a_value_that_wrapped_goes_with_its_head() {
    let wrapped = "\
AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCY
EXAMPLEKEY000000000000
region=eu-west-1
";
    let out = scan(wrapped);
    assert!(
        !out.text.contains("EXAMPLEKEY000000000000"),
        "the second half of the key survived:\n{}",
        out.text
    );
    assert!(out.text.contains("region=eu-west-1"), "{}", out.text);

    // Base64 that wrapped: the tail carries the padding, and padding is
    // the one character that also separates a name from a value.
    let base64 = "token: aGVsbG8td29ybGQtdGhpcy1pcy1ub3QtYS1yZWFs\nLXRva2VuLWF0LWFsbC1ub3Bl=\nthat is the whole file\n";
    let out = scan(base64);
    assert!(
        !out.text.contains("LXRva2VuLWF0LWFsbC1ub3Bl"),
        "the second half of the value survived:\n{}",
        out.text
    );
    assert!(out.text.contains("that is the whole file"), "{}", out.text);

    // The control. A line under a secret is only the rest of it when it
    // is nothing but credential characters: a sentence, an indented
    // field and one plain word are all left where they are.
    // A payload that arrived from a machine that ends its lines the
    // other way is the same payload.
    let crlf = "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCY\r\nEXAMPLEKEY000000000000\r\nregion=eu-west-1\r\n";
    let out = scan(crlf);
    assert!(
        !out.text.contains("EXAMPLEKEY000000000000"),
        "the second half survived a CRLF break:\n{}",
        out.text
    );
    assert!(out.text.contains("region=eu-west-1"), "{}", out.text);

    let after = "API_TOKEN=EXAMPLE-NOT-A-REAL-TOKEN-0000\nand the deployment finished\ndocumentation\n";
    let out = scan(after);
    assert!(out.text.contains("and the deployment finished"), "{}", out.text);
    assert!(out.text.contains("documentation"), "{}", out.text);
    assert_eq!(out.findings.len(), 1, "{:?}", out.findings);
}

/// The scan runs on every turn that leaves the machine, so it is allowed
/// to be thorough and not allowed to be slow. The payload below is the
/// shape that punishes a rule which reaches forward from what it found:
/// a page of base64 under one labelled name, where every line is both a
/// candidate of its own and the continuation of the line above it.
///
/// The bound is loose on purpose — this is not a benchmark, and a busy
/// machine is not a failure. What it catches is the change that turns
/// one pass over the payload into one pass per line, which on this input
/// is not slower by a factor of two but by a factor of forty thousand.
#[test]
fn a_large_payload_is_scanned_in_one_pass() {
    let mut text = String::from("token: aGVsbG8td29ybGQtdGhpcy1pcy1ub3QtYS1yZWFs\n");
    for n in 0..40_000 {
        text.push_str("aGVsbG8gd29ybGQgdGhpcyBpcyBub3QgcmVhbA");
        text.push_str(&format!("{n:04}\n"));
    }

    let began = std::time::Instant::now();
    let out = scan(&text);
    let took = began.elapsed();

    assert!(
        took.as_secs() < 5,
        "{} bytes took {took:?}",
        text.len()
    );
    // And it is one value, not forty thousand: what the user reads on
    // the manifest is "a value written under token", once.
    assert_eq!(out.findings.len(), 1, "{:?}", out.findings);
    assert!(!out.text.contains("aGVsbG8gd29ybGQ"), "the page survived");
}

/// A payload that had something taken out says so, in words the far
/// model can act on. Without that it reads a marker as noise and answers
/// as though nothing was missing — which is the failure this note exists
/// to prevent.
#[test]
fn the_payload_tells_the_far_model_that_something_was_withheld() {
    let clean = Gathering::new()
        .with_text("which theme is installed?")
        .unreviewed();
    assert!(clean.is_clean());
    assert!(!clean.payload().contains("[[note:"), "{}", clean.payload());

    let dirty = Gathering::new()
        .with_text("the token is ghp_EXAMPLE_NOT_A_REAL_TOKEN_00000000")
        .unreviewed();
    let payload = dirty.payload();
    assert!(payload.contains("ask the user for it"), "{payload}");
    assert!(payload.contains("Do not guess"), "{payload}");
}

// ---- layer 3: the model, last ---------------------------------------

/// A reviewer that returns exactly what a test needs it to.
struct Scripted(Review);

impl Reviewer for Scripted {
    fn review(&mut self, _payload: &str) -> Review {
        self.0.clone()
    }
}

/// What layer 3 is for: meaning, which no pattern can see.
#[test]
fn the_local_review_can_remove_what_the_patterns_could_not_see() {
    let out = Gathering::new()
        .with_text("Ask about the deployment. Michael's MRI is on Thursday.")
        .reviewed(&mut Scripted(Review {
            removals: vec![Removal::new(
                "Michael's MRI is on Thursday.",
                "a medical appointment",
            )],
            note: None,
        }));

    assert!(!out.payload().contains("MRI"), "{}", out.payload());
    assert!(out.payload().contains("a medical appointment"), "{}", out.payload());
    assert!(out.payload().contains("Ask about the deployment."));
    assert_eq!(out.removals().len(), 1);
}

/// The rule the whole ordering rests on. A reviewer is handed the
/// redacted text and asked what else to remove; there is no shape of
/// answer that puts a secret back, and a reviewer that tries — by
/// quoting the marker and putting the original value in its reason —
/// gets a payload with neither.
#[test]
fn the_review_cannot_restore_what_the_patterns_removed() {
    let secret = "ghp_EXAMPLE_NOT_A_REAL_TOKEN_00000000";
    let out = Gathering::new()
        .with_text(&format!("the deploy key is {secret}"))
        .reviewed(&mut Scripted(Review {
            removals: vec![Removal::new(
                "[[redacted: a GitHub token — removed by the local agent before this left the machine]]",
                // The reason is the reviewer's own text, and a reviewer
                // that echoes the value into it must not thereby put it
                // back: the reason is scanned like anything else.
                format!("this was fine, it was only {secret}"),
            )],
            note: None,
        }));

    let payload = out.payload();
    assert!(!payload.contains(secret), "the secret came back: {payload}");
    assert!(payload.contains("[[redacted:"), "{payload}");
}

/// Asking a remote model whether a payload may be sent would send it.
#[test]
fn a_reviewer_cannot_be_built_on_a_backend_that_is_not_local() {
    struct Remote;
    impl Backend for Remote {
        fn name(&self) -> &str {
            "somewhere-else"
        }
        fn send(&mut self, _r: &Request, _s: &mut EventSink<'_>) -> Result<(), BackendError> {
            panic!("a remote reviewer must never be given a payload to send");
        }
    }

    let err = LocalReviewer::new(Box::new(Remote), "any-model").expect_err("must be refused");
    assert!(err.to_string().contains("not a local backend"), "{err}");
}

// ---- layer 4: the manifest ------------------------------------------

fn payload_with_a_file(path: &Path) -> Gathering {
    Gathering::new()
        .with_text("the user asked why the desktop starts with no panels")
        .with_file(
            path,
            "Theme=crimson\nAPI_TOKEN=EXAMPLE-NOT-A-REAL-TOKEN-0000\n",
        )
}

/// The manifest is what makes the three layers underneath auditable, so
/// it has to name the files, the size, and what went — and it must not
/// itself be another copy of the payload.
#[test]
fn the_manifest_says_what_leaves_and_never_prints_it() {
    let path = PathBuf::from("/home/michael/.config/nacelle-desktop/nacelle-desktop.conf");
    let out = payload_with_a_file(&path).reviewed(&mut NoReview);
    let manifest = out.manifest("anthropic", "the local model failed twice", Why::FirstEscalation);
    let shown = manifest.render();

    assert!(shown.contains("anthropic"), "{shown}");
    assert!(shown.contains("nacelle-desktop.conf"), "{shown}");
    assert!(shown.contains(&format!("{} bytes", out.bytes())), "{shown}");
    assert!(shown.contains("Removed before sending"), "{shown}");
    assert!(
        !shown.contains("EXAMPLE-NOT-A-REAL-TOKEN-0000"),
        "the manifest must not be a second copy of the secret: {shown}"
    );
    assert_eq!(manifest.sources.len(), 1);
    assert_eq!(manifest.sources[0].removed, 1);
}

/// A manifest that appears before every escalation is a manifest that is
/// clicked through. It appears when there is something new to see, and
/// [`Disclosure`] is what knows when that is.
#[test]
fn the_manifest_is_shown_first_and_then_only_for_files_not_yet_seen() {
    let first = PathBuf::from("/home/michael/notes/one.txt");
    let second = PathBuf::from("/home/michael/notes/two.txt");
    let mut disclosure = Disclosure::new();

    let one = Gathering::new()
        .with_file(&first, "nothing sensitive")
        .unreviewed();
    assert_eq!(
        disclosure.required_for(one.sources()),
        Some(Why::FirstEscalation)
    );
    disclosure.accepted(one.sources());

    // The same file again is not news.
    let again = Gathering::new()
        .with_file(&first, "nothing sensitive")
        .unreviewed();
    assert_eq!(disclosure.required_for(again.sources()), None);

    // Conversation-only escalations are not news either.
    let talk = Gathering::new()
        .with_text("and what about the other one?")
        .unreviewed();
    assert_eq!(disclosure.required_for(talk.sources()), None);

    // A file the user has not seen is.
    let new = Gathering::new()
        .with_file(&second, "also nothing sensitive")
        .unreviewed();
    assert_eq!(disclosure.required_for(new.sources()), Some(Why::UnseenFile));

    // Unless they have already read it in the conversation.
    disclosure.seen_already(&second);
    assert_eq!(disclosure.required_for(new.sources()), None);
}

/// A layer that did not run and a layer that found nothing look the same
/// on a manifest that does not distinguish them. They are not the same
/// assurance, so the manifest says which it was.
#[test]
fn the_manifest_admits_when_the_local_review_did_not_run() {
    let unreviewed = Gathering::new().with_text("anything at all").unreviewed();
    let shown = unreviewed
        .manifest("anthropic", "you asked", Why::FirstEscalation)
        .render();
    assert!(shown.contains("no local model reviewed this"), "{shown}");

    let failed = Gathering::new()
        .with_text("anything at all")
        .reviewed(&mut Scripted(Review::failed("the local model was not running")));
    let shown = failed
        .manifest("anthropic", "you asked", Why::FirstEscalation)
        .render();
    assert!(shown.contains("the local model was not running"), "{shown}");
}

/// The ordering, as the type rather than as a flag.
///
/// Half of this cannot be asserted here, and that is the point: there is
/// no `with_text` on what [`Gathering::reviewed`] hands back, so the code
/// that would add to a reviewed payload is a compiler error rather than a
/// failing test. That half lives as a `compile_fail` doctest on the
/// `redact` module, next to the one that shows the same call working
/// before the review. What is left for a test is the consequence: a
/// payload finished by layer 3, and a manifest whose figures came out of
/// the finished thing rather than out of a running total.
#[test]
fn what_the_manifest_says_about_a_file_is_read_off_the_payload_that_leaves() {
    let path = PathBuf::from("/home/michael/notes/consult.txt");
    let contents = "Theme=crimson\nthe diagnosis was pneumonia\n";

    let out = Gathering::new()
        .with_text("what should I do about this?")
        .with_file(&path, contents)
        .reviewed(&mut Scripted(Review {
            removals: vec![Removal::new(
                "the diagnosis was pneumonia",
                "a medical detail",
            )],
            note: None,
        }));

    let manifest = out.manifest("anthropic", "you asked", Why::FirstEscalation);
    assert_eq!(manifest.sources.len(), 1);
    let source = &manifest.sources[0];

    // What layer 3 took came out of THIS file, so this file's own line on
    // the manifest moved with it. The shape before this one wrote the
    // count down when the file went in and never looked again, which made
    // every figure on the manifest a description of a payload that had
    // since changed.
    assert_eq!(source.path, path);
    assert_eq!(source.bytes_read, contents.len());
    assert_eq!(
        source.removed, 1,
        "the removal happened inside this file and is not on its line"
    );
    assert_ne!(
        source.bytes_sent, contents.len(),
        "a file that had a sentence replaced cannot send exactly what was read"
    );

    // Byte for byte: `bytes_sent` is the length of this file's contents
    // where they actually sit in the payload, marker and all.
    let head = format!("--- {} ---\n", path.display());
    let at = out.payload().find(&head).expect("the file's own piece") + head.len();
    let sent = &out.payload()[at..at + source.bytes_sent];
    assert!(sent.starts_with("Theme=crimson"), "{sent}");
    assert!(sent.contains("a medical detail"), "{sent}");
    assert!(!sent.contains("pneumonia"), "{sent}");
    // And nothing of this file's beyond that: what follows is the note
    // that says something was withheld, which belongs to the payload
    // rather than to the file.
    assert!(
        out.payload()[at + source.bytes_sent..].starts_with("\n[[note:"),
        "{}",
        &out.payload()[at + source.bytes_sent..]
    );

    // The size of the whole is the length of the one string that goes,
    // not a sum kept beside it.
    assert_eq!(manifest.bytes, out.payload().len());
}

/// A review that quotes something spanning two pieces removes nothing,
/// and the manifest says nothing was removed. The alternative — counting
/// what was asked for rather than what came out — is a manifest that
/// reports a redaction that never happened, which is the same lie as
/// missing one, told in the reassuring direction.
#[test]
fn a_removal_is_counted_only_where_it_actually_came_out() {
    let out = Gathering::new()
        .with_text("the first half of the sentence")
        .with_text("and the second half")
        .reviewed(&mut Scripted(Review {
            removals: vec![Removal::new(
                "the first half of the sentence\n\nand the second half",
                "a private matter",
            )],
            note: None,
        }));

    assert!(out.removals().is_empty(), "{:?}", out.removals());
    assert!(out.is_clean());
    assert!(out.payload().contains("the first half of the sentence"));
    let manifest = out.manifest("anthropic", "you asked", Why::FirstEscalation);
    assert!(manifest.removed.is_empty(), "{:?}", manifest.removed);
    assert_eq!(manifest.bytes, out.payload().len());
}

