//! The rules that hold when the input is hostile.
//!
//! A tool argument arrives from a language model, and a language model
//! repeats what it was told — including by a web page, a file it was
//! asked to summarise, or a user who read a clever suggestion
//! somewhere. So these tests do not ask whether the tools work; they
//! ask what happens when the name is `../../etc/passwd`, when a file
//! inside the data directory is a symlink pointing out of it, when a
//! value carries a newline, and when the write cannot finish.
//!
//! Three properties are being pinned down:
//!
//! * **Confinement.** Every path is resolved canonically and refused
//!   unless it lands inside the directory the tool is allowed in.
//! * **Nothing on a refusal.** A rejected call leaves the file byte for
//!   byte as it was, and leaves no temporary file behind either.
//! * **Atomicity.** A write that cannot finish leaves the original
//!   intact, and a write that does finish leaves the previous contents
//!   in a backup.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use nacelle_ai::tools::paths::confine;
use nacelle_ai::tools::write;
use nacelle_ai::tools::{TOOL_READ_LAYAUT, TOOL_SET_CONFIG, TOOL_SET_LAYAUT, TOOL_SET_THEME};
use nacelle_ai::{DesktopDirs, ToolError, Toolbox};
use serde_json::json;

/// The family's folder. The configuration file keeps the program's
/// name, which is why only one of these two reads "desktop".
const APP: &str = "nacelle";
const CONF: &str = "nacelle-desktop.conf";
/// The RON document — the only file a write may produce. The old
/// `Key=Value` file is read, never written.
const CONF_RON: &str = "nacelle-desktop.ron";

fn scratch(tag: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "nacelle-ai-safety-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

struct Install {
    root: PathBuf,
    config: PathBuf,
    data: PathBuf,
}

fn install(tag: &str) -> Install {
    let root = scratch(tag);
    let config = root.join("config").join(APP);
    let data = root.join("data").join(APP);
    fs::create_dir_all(&config).expect("config directory");
    fs::create_dir_all(&data).expect("data directory");
    Install { root, config, data }
}

impl Install {
    fn toolbox(&self) -> Toolbox {
        Toolbox::new(DesktopDirs::new(
            Some(self.config.clone()),
            Some(self.data.clone()),
        ))
    }

    fn conf(&self) -> PathBuf {
        self.config.join(CONF)
    }

    fn conf_ron(&self) -> PathBuf {
        self.config.join(CONF_RON)
    }

    /// Something worth stealing, outside both directories.
    fn secret(&self) -> PathBuf {
        let path = self.root.join("outside").join("secret");
        fs::create_dir_all(path.parent().expect("a parent")).expect("outside directory");
        fs::write(&path, "TOP SECRET\n").expect("write secret");
        path
    }
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) {
    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent).expect("link directory");
    }
    std::os::unix::fs::symlink(target, link).expect("symlink");
}

/// Every file in `dir`, including the hidden ones a temporary file
/// would be.
fn entries(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(dir)
        .expect("read directory")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

// ---- confinement ---------------------------------------------------

#[test]
fn confine_refuses_every_way_out_and_allows_a_plain_name() {
    let root = scratch("confine");
    let inside = root.join("inside");
    fs::create_dir_all(&inside).expect("inside");
    fs::write(inside.join("real"), "here").expect("a real file");
    let outside = root.join("outside");
    fs::create_dir_all(&outside).expect("outside");
    fs::write(outside.join("secret"), "there").expect("a secret");

    // A file that exists, and one that does not yet: both are allowed
    // as long as they land inside.
    assert_eq!(
        confine(&inside, &inside.join("real")).expect("a real file inside"),
        inside.join("real").canonicalize().expect("canonical")
    );
    assert!(confine(&inside, &inside.join("new")).is_ok(), "a new file");

    for escape in [
        inside.join("..").join("outside").join("secret"),
        outside.join("secret"),
        PathBuf::from("/etc/passwd"),
        inside.join(".."),
    ] {
        let err = confine(&inside, &escape).expect_err("must be refused");
        assert!(
            matches!(err, ToolError::Outside { .. } | ToolError::Io { .. }),
            "{} gave {err:?}",
            escape.display()
        );
    }

    // A symlink is followed before the check, so a link inside the
    // directory that points out of it is caught by the same rule.
    #[cfg(unix)]
    {
        symlink(&outside.join("secret"), &inside.join("link"));
        let err = confine(&inside, &inside.join("link")).expect_err("must be refused");
        assert!(matches!(err, ToolError::Outside { .. }), "{err:?}");
    }
}

#[test]
fn a_layaut_name_that_is_a_path_is_refused() {
    let install = install("layaut-path");
    let secret = install.secret();
    let tools = install.toolbox();

    for bad in [
        "../../outside/secret",
        "..",
        ".",
        "/etc/passwd",
        "sub/wide",
        "back\\slash",
        "",
        "   ",
    ] {
        let err = tools
            .run(TOOL_READ_LAYAUT, &json!({ "name": bad }))
            .expect_err("must be refused");
        assert!(
            matches!(err, ToolError::Rejected { .. }),
            "{bad:?} gave {err:?}"
        );
        let message = err.to_string();
        assert!(
            !message.contains("TOP SECRET"),
            "an error must not carry file contents: {message}"
        );
    }
    assert_eq!(
        fs::read_to_string(&secret).expect("still there"),
        "TOP SECRET\n"
    );
}

/// The name is a legitimate one; the FILE is a link out of the data
/// directory. Canonical resolution is what catches this, and it is the
/// case a check on the name alone would miss.
#[cfg(unix)]
#[test]
fn a_layaut_symlinked_out_of_the_data_directory_is_refused() {
    let install = install("layaut-link");
    let secret = install.secret();
    symlink(
        &secret,
        &install.data.join("layauts").join("innocent.layaut"),
    );

    let err = install
        .toolbox()
        .run(TOOL_READ_LAYAUT, &json!({ "name": "innocent" }))
        .expect_err("must be refused");
    assert!(matches!(err, ToolError::Outside { .. }), "{err:?}");
    assert!(
        !err.to_string().contains("TOP SECRET"),
        "the refusal must not leak what it refused to read"
    );
}

#[test]
fn a_theme_or_layaut_name_that_is_a_path_never_reaches_the_file() {
    let install = install("name-path");
    fs::write(install.conf(), "Theme=aurora\nLayaut=wide\n").expect("conf");

    for (tool, name) in [
        (TOOL_SET_THEME, "../../../etc/passwd"),
        (TOOL_SET_THEME, "aurora/../../evil"),
        (TOOL_SET_LAYAUT, "../wide"),
        (TOOL_SET_LAYAUT, "/etc/passwd"),
    ] {
        let err = install
            .toolbox()
            .run(tool, &json!({ "name": name }))
            .expect_err("must be refused");
        assert!(
            matches!(err, ToolError::Rejected { .. } | ToolError::NotFound { .. }),
            "{tool} {name:?} gave {err:?}"
        );
    }
    assert_eq!(
        fs::read_to_string(install.conf()).expect("read back"),
        "Theme=aurora\nLayaut=wide\n",
        "a refused call writes nothing"
    );
}

/// A value carrying a newline would end the line early and turn the
/// rest into a second `Key=Value` that nothing validated: one tool call
/// writing two settings.
#[test]
fn a_newline_in_a_value_cannot_smuggle_a_second_setting() {
    let install = install("injection");
    fs::write(install.conf(), "SoundVolume=40\n").expect("conf");

    let err = install
        .toolbox()
        .run(
            TOOL_SET_CONFIG,
            &json!({ "key": "TermFontFamily", "value": "Mono\nTheme=evil" }),
        )
        .expect_err("must be refused");
    assert!(matches!(err, ToolError::Rejected { .. }), "{err:?}");
    let text = fs::read_to_string(install.conf()).expect("read back");
    assert_eq!(text, "SoundVolume=40\n");
    assert!(!text.contains("Theme"), "no second setting was written");
}

// ---- nothing on a refusal, everything on a success -----------------

#[test]
fn a_refused_call_leaves_the_file_and_the_directory_exactly_as_they_were() {
    let install = install("untouched");
    let before = "# careful\nTheme = crimson\nSoundVolume=40\n";
    fs::write(install.conf(), before).expect("conf");
    let listing = entries(&install.config);

    let err = install
        .toolbox()
        .run(TOOL_SET_CONFIG, &json!({ "key": "SoundVolume", "value": "900" }))
        .expect_err("must be refused");
    assert!(matches!(err, ToolError::Rejected { .. }), "{err:?}");

    assert_eq!(fs::read_to_string(install.conf()).expect("read back"), before);
    assert_eq!(
        entries(&install.config),
        listing,
        "no backup and no temporary file may be left behind"
    );
}

#[test]
fn an_overwrite_keeps_the_previous_contents_in_a_backup() {
    let install = install("backup");
    install
        .toolbox()
        .run(TOOL_SET_THEME, &json!({ "name": "crimson" }))
        .expect("the first write must succeed");
    let before = fs::read_to_string(install.conf_ron()).expect("the first document");

    install
        .toolbox()
        .run(TOOL_SET_THEME, &json!({ "name": "aurora" }))
        .expect("the write must succeed");

    let backup = install.config.join(format!("{CONF_RON}.bak"));
    assert_eq!(fs::read_to_string(&backup).expect("a backup"), before);
    assert!(
        fs::read_to_string(install.conf_ron())
            .expect("read back")
            .contains("aurora"),
        "the document now names the new theme"
    );
    assert_eq!(
        entries(&install.config),
        vec![CONF_RON.to_string(), format!("{CONF_RON}.bak")],
        "the temporary file must not survive the rename"
    );
}

/// The first write to a fresh installation has nothing to back up, and
/// says so rather than naming a file it never wrote.
#[test]
fn a_first_write_reports_no_backup() {
    let install = install("first-write");
    let text = install
        .toolbox()
        .run(TOOL_SET_THEME, &json!({ "name": "aurora" }))
        .expect("the write must succeed");
    let out: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(out["backup"], serde_json::Value::Null);
    assert_eq!(out["previous"], serde_json::Value::Null);
    assert_eq!(entries(&install.config), vec![CONF_RON.to_string()]);
}

/// An old file that declares the same key twice must not confuse the
/// write: the desktop's old reader took the LAST declaration, so the
/// seed does too, and the document written says exactly one thing.
#[test]
fn a_key_declared_twice_is_left_saying_one_thing() {
    let install = install("duplicate");
    let before = "Theme=crimson\nSoundVolume=40\nTheme=lockdown\n";
    fs::write(install.conf(), before).expect("conf");

    let text = install
        .toolbox()
        .run(TOOL_SET_THEME, &json!({ "name": "aurora" }))
        .expect("the write must succeed");
    let out: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(
        out["previous"], "lockdown",
        "the last declaration is the one the desktop read, so it is the previous value"
    );

    let doc = fs::read_to_string(install.conf_ron()).expect("read back");
    assert_eq!(doc.matches("aurora").count(), 1, "{doc}");
    assert!(!doc.contains("crimson") && !doc.contains("lockdown"), "{doc}");
    assert_eq!(
        fs::read_to_string(install.conf()).expect("old file"),
        before,
        "the old file is read, never rewritten — duplicates and all"
    );
}

// ---- atomicity -----------------------------------------------------

/// A write that cannot finish must leave the original file exactly as
/// it was — not truncated, not half-written, and with no temporary file
/// left in the directory.
///
/// The failure is simulated by taking write permission away from the
/// configuration directory, which is the closest a test can get to "the
/// filesystem said no" without a mock filesystem. Running as root
/// ignores those bits, so the test checks that the denial is real
/// before asserting anything about it.
#[cfg(unix)]
#[test]
fn a_write_that_cannot_finish_leaves_the_original_intact() {
    use std::os::unix::fs::PermissionsExt;

    let install = install("atomic");
    let before = "Theme=crimson\nSoundVolume=40\n";
    fs::write(install.conf(), before).expect("conf");
    let listing = entries(&install.config);

    fs::set_permissions(&install.config, fs::Permissions::from_mode(0o555))
        .expect("make the directory read-only");
    let denied = fs::write(install.config.join(".probe"), "x").is_err();

    if denied {
        let err = install
            .toolbox()
            .run(TOOL_SET_THEME, &json!({ "name": "aurora" }))
            .expect_err("the write cannot succeed");
        assert!(matches!(err, ToolError::Io { .. }), "{err:?}");
        assert_eq!(
            fs::read_to_string(install.conf()).expect("read back"),
            before,
            "the original must survive a failed write untouched"
        );
        assert_eq!(entries(&install.config), listing, "no debris may be left");
    }

    fs::set_permissions(&install.config, fs::Permissions::from_mode(0o755))
        .expect("restore permissions");
    assert!(denied, "the test needs a directory it really cannot write to");
}

/// The mechanism on its own: a replacement is a rename, so a reader
/// racing it sees the old file or the new one, and the file it opened
/// is never truncated under it.
#[test]
fn a_replacement_never_shortens_the_file_in_place() {
    let install = install("replace");
    let path = install.config.join("sample");
    fs::write(&path, "the original, which is quite long\n").expect("sample");
    let opened = fs::File::open(&path).expect("a reader holds it open");

    let done = write::replace(&path, "short\n").expect("replace");
    assert_eq!(done.backup, Some(install.config.join("sample.bak")));
    assert_eq!(fs::read_to_string(&path).expect("read back"), "short\n");

    // The handle a reader opened before the rename still names the old
    // inode, whole. That is what the rename bought.
    let mut kept = String::new();
    std::io::Read::read_to_string(&mut { opened }, &mut kept).expect("read the old inode");
    assert_eq!(kept, "the original, which is quite long\n");
}

/// The replacement inherits the mode of what it replaced, so a file the
/// user tightened does not come back loosened.
#[cfg(unix)]
#[test]
fn a_replacement_keeps_the_permissions_of_what_it_replaced() {
    use std::os::unix::fs::PermissionsExt;

    let install = install("modes");
    let path = install.config.join("sample");
    fs::write(&path, "before\n").expect("sample");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("tighten");

    write::replace(&path, "after\n").expect("replace");
    let mode = fs::metadata(&path).expect("metadata").permissions().mode();
    assert_eq!(mode & 0o777, 0o600, "the mode must survive the replacement");
}
