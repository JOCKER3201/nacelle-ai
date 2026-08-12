//! Every tool, on a throwaway installation.
//!
//! Each test builds a whole nacelle-desktop directory tree in a
//! temporary directory and points a [`Toolbox`] at it. Nothing reads
//! the process environment, nothing touches the developer's own
//! `~/.config`, and no test depends on what happens to be installed on
//! the machine running it — which matters here more than usual, since
//! the machine running these tests may well have a real nacelle desktop
//! on it.
//!
//! The directories are handed over explicitly rather than through
//! `XDG_*` for the same reason: `DesktopDirs::from_env` adds the system
//! search path, and `/usr/share/nacelle-desktop` existing or not would
//! decide whether a listing test passed. The two tests that are ABOUT
//! the search path build their own environment map, with every system
//! directory pointed at a temporary one.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use nacelle_ai::message::ToolCall;
use nacelle_ai::tools::{
    TOOL_LIST_ADDONS, TOOL_LIST_LAYAUTS, TOOL_LIST_THEMES, TOOL_READ_CONFIG, TOOL_READ_LAYAUT,
    TOOL_SET_CONFIG, TOOL_SET_LAYAUT, TOOL_SET_THEME,
};
use nacelle_ai::{Content, DesktopDirs, ToolError, Toolbox};
use serde_json::{json, Value};

const APP: &str = "nacelle-desktop";
const CONF: &str = "nacelle-desktop.conf";

/// A fresh directory for one test. The counter keeps parallel tests
/// from colliding without needing a lock.
fn scratch(tag: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "nacelle-ai-tools-{tag}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

/// One installation: `<root>/config/nacelle-desktop` and
/// `<root>/data/nacelle-desktop`, both created.
struct Install {
    config: PathBuf,
    data: PathBuf,
}

fn install(tag: &str) -> Install {
    let root = scratch(tag);
    let config = root.join("config").join(APP);
    let data = root.join("data").join(APP);
    fs::create_dir_all(&config).expect("config directory");
    fs::create_dir_all(&data).expect("data directory");
    Install { config, data }
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

    fn write_conf(&self, body: &str) {
        fs::write(self.conf(), body).expect("write conf");
    }

    /// A file under the data directory, parents created.
    fn data_file(&self, rel: &str, body: &str) -> PathBuf {
        let path = self.data.join(rel);
        fs::create_dir_all(path.parent().expect("a parent")).expect("data subdirectory");
        fs::write(&path, body).expect("write data file");
        path
    }
}

fn run(tools: &Toolbox, name: &str, input: Value) -> Value {
    let text = tools
        .run(name, &input)
        .unwrap_or_else(|e| panic!("{name} failed: {e}"));
    serde_json::from_str(&text).expect("a tool result is JSON")
}

fn names(list: &Value, field: &str) -> Vec<String> {
    list.as_array()
        .expect("an array")
        .iter()
        .map(|e| e[field].as_str().expect("a string").to_string())
        .collect()
}

fn conf_value(path: &Path, key: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .filter_map(|l| l.trim().split_once('='))
        .filter(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim().to_string())
        .next_back()
}

// ---- the declarations ---------------------------------------------

#[test]
fn every_tool_is_declared_once_with_an_object_schema() {
    let tools = install("declare").toolbox();
    let declared = tools.declarations();
    let mut names: Vec<&str> = declared.iter().map(|d| d.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            TOOL_LIST_ADDONS,
            TOOL_LIST_LAYAUTS,
            TOOL_LIST_THEMES,
            TOOL_READ_CONFIG,
            TOOL_READ_LAYAUT,
            TOOL_SET_CONFIG,
            TOOL_SET_LAYAUT,
            TOOL_SET_THEME,
        ]
    );
    for d in &declared {
        assert_eq!(d.input_schema["type"], "object", "{}", d.name);
        assert!(!d.description.is_empty(), "{}", d.name);
    }
}

/// The one thing the model must never get wrong: these tools change
/// files, and a desktop that is already running does not notice.
#[test]
fn every_writing_tool_says_the_change_is_not_live() {
    let tools = install("promise").toolbox();
    for name in [TOOL_SET_THEME, TOOL_SET_LAYAUT, TOOL_SET_CONFIG] {
        let d = tools
            .declarations()
            .into_iter()
            .find(|d| d.name == name)
            .expect("declared");
        assert!(
            d.description
                .contains("does NOT change a desktop that is already running"),
            "{name} must warn that the change is not live: {}",
            d.description
        );
        assert!(
            d.description.contains("next time the desktop starts"),
            "{name} must say when the change applies"
        );
    }
}

// ---- themes --------------------------------------------------------

#[test]
fn themes_are_listed_sorted_with_the_active_one_named() {
    let install = install("themes");
    install.data_file("themes/crimson.theme", "[meta]\n");
    install.data_file("themes/aurora.theme", "[meta]\n");
    install.data_file("themes/notes.txt", "not a theme");
    install.write_conf("Theme=crimson\n");

    let out = run(&install.toolbox(), TOOL_LIST_THEMES, json!({}));
    assert_eq!(names(&out["installed"], "name"), ["aurora", "crimson"]);
    assert_eq!(out["active"], "crimson");
    assert_eq!(out["active_from"], install.conf().display().to_string());
    assert!(
        out["note"].as_str().expect("a note").contains("compiled"),
        "the listing must admit it cannot see the built-in themes"
    );
}

#[test]
fn choosing_a_theme_writes_the_key_and_keeps_the_rest_of_the_file() {
    let install = install("set-theme");
    install.data_file("themes/aurora.theme", "[meta]\n");
    install.write_conf("# mine\nSoundVolume=40\nTheme=crimson\n");

    let out = run(
        &install.toolbox(),
        TOOL_SET_THEME,
        json!({ "name": "aurora" }),
    );
    assert_eq!(out["value"], "aurora");
    assert_eq!(out["previous"], "crimson");
    assert!(out.get("warning").is_none(), "aurora is installed");

    let text = fs::read_to_string(install.conf()).expect("read back");
    assert!(text.contains("# mine"), "comments survive: {text}");
    assert!(text.contains("SoundVolume=40"), "other keys survive: {text}");
    assert_eq!(conf_value(&install.conf(), "Theme").as_deref(), Some("aurora"));
}

/// A theme that is not a file may still be one of the toolkit's own, so
/// this is a warning attached to a completed write, not a refusal.
#[test]
fn choosing_a_theme_with_no_file_warns_but_still_writes() {
    let install = install("set-theme-builtin");
    let out = run(
        &install.toolbox(),
        TOOL_SET_THEME,
        json!({ "name": "lockdown" }),
    );
    assert_eq!(out["value"], "lockdown");
    assert!(
        out["warning"]
            .as_str()
            .expect("a warning")
            .contains("compiled into the toolkit"),
        "{out}"
    );
    assert_eq!(
        conf_value(&install.conf(), "Theme").as_deref(),
        Some("lockdown")
    );
}

#[test]
fn a_theme_name_that_is_not_a_bare_identifier_is_refused() {
    let install = install("theme-name");
    install.write_conf("Theme=aurora\n");
    for bad in ["../evil", "a/b", "with space", "quote\"d"] {
        let err = install
            .toolbox()
            .run(TOOL_SET_THEME, &json!({ "name": bad }))
            .expect_err("must be refused");
        assert!(
            matches!(err, ToolError::Rejected { .. }),
            "{bad:?} gave {err:?}"
        );
    }
    assert_eq!(
        conf_value(&install.conf(), "Theme").as_deref(),
        Some("aurora"),
        "a refusal must leave the setting alone"
    );
}

// ---- layauts -------------------------------------------------------

#[test]
fn layauts_are_listed_with_the_built_in_one_first() {
    let install = install("layauts");
    install.data_file("layauts/wide.layaut", "[column]\n");
    install.data_file("layauts/narrow.layaut", "[column]\n");
    install.write_conf("Layaut=wide\n");

    let out = run(&install.toolbox(), TOOL_LIST_LAYAUTS, json!({}));
    assert_eq!(
        names(&out["installed"], "name"),
        ["default", "narrow", "wide"]
    );
    let entries = out["installed"].as_array().expect("an array");
    assert_eq!(entries[0]["built_in"], true);
    assert_eq!(entries[0]["path"], Value::Null);
    assert_eq!(out["active"], "wide");
}

#[test]
fn a_layaut_is_read_as_text() {
    let install = install("read-layaut");
    let body = "units = du\n[column]\nbasis = 16.4\npanel = cpu 26\n";
    let path = install.data_file("layauts/wide.layaut", body);

    let out = run(
        &install.toolbox(),
        TOOL_READ_LAYAUT,
        json!({ "name": "wide" }),
    );
    assert_eq!(out["text"], body);
    assert_eq!(out["path"], path.display().to_string());
}

#[test]
fn the_built_in_layaut_reports_that_it_has_no_file() {
    let install = install("read-default");
    let out = run(
        &install.toolbox(),
        TOOL_READ_LAYAUT,
        json!({ "name": "default" }),
    );
    assert_eq!(out["text"], Value::Null);
    assert!(out["note"]
        .as_str()
        .expect("a note")
        .contains("has no file"));
}

#[test]
fn reading_a_layaut_that_is_not_installed_says_so() {
    let install = install("read-missing");
    let err = install
        .toolbox()
        .run(TOOL_READ_LAYAUT, &json!({ "name": "nope" }))
        .expect_err("must fail");
    assert!(matches!(err, ToolError::NotFound { .. }), "{err:?}");
}

#[test]
fn choosing_a_layaut_that_is_not_installed_is_refused_before_the_write() {
    let install = install("set-layaut-missing");
    install.data_file("layauts/wide.layaut", "[column]\n");
    install.write_conf("Layaut=wide\n");

    let err = install
        .toolbox()
        .run(TOOL_SET_LAYAUT, &json!({ "name": "nope" }))
        .expect_err("must fail");
    match &err {
        ToolError::NotFound { what } => assert!(
            what.contains("wide") && what.contains("default"),
            "the error must list what IS installed: {what}"
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(conf_value(&install.conf(), "Layaut").as_deref(), Some("wide"));
}

#[test]
fn choosing_an_installed_layaut_writes_the_key() {
    let install = install("set-layaut");
    install.data_file("layauts/narrow.layaut", "[column]\n");
    let out = run(
        &install.toolbox(),
        TOOL_SET_LAYAUT,
        json!({ "name": "narrow" }),
    );
    assert_eq!(out["value"], "narrow");
    assert_eq!(out["previous"], Value::Null);
    assert_eq!(
        conf_value(&install.conf(), "Layaut").as_deref(),
        Some("narrow")
    );
}

/// Clearing is a real operation: `Layaut=` means "the built-in one",
/// and it has to beat a system file that names another.
#[test]
fn an_empty_name_clears_the_layaut_setting() {
    let install = install("clear-layaut");
    install.write_conf("Layaut=wide\n");
    let out = run(&install.toolbox(), TOOL_SET_LAYAUT, json!({ "name": "" }));
    assert_eq!(out["value"], "");
    assert_eq!(conf_value(&install.conf(), "Layaut").as_deref(), Some(""));
}

// ---- addons --------------------------------------------------------

#[test]
fn addons_report_what_they_declare_and_nothing_more() {
    let install = install("addons");
    install.data_file(
        "addons/scripts/sysinfo.rhai",
        "// label: SYSTEM INFO\n// category: appgrid\n// ref_h: 4.5\nfn draw() { [] }\n",
    );
    install.data_file("addons/scripts/bare.rhai", "fn draw() { [] }\n");
    install.data_file("addons/scripts/notes.txt", "// label: NOPE\n");
    install.data_file("addons/plugins/meter.so", "not really a library");
    install.data_file("addons/plugins/meter.meta", "label = METER\nmin_h = 3.5\n");
    install.data_file("addons/plugins/plain.so", "not really a library");

    let out = run(&install.toolbox(), TOOL_LIST_ADDONS, json!({}));
    let listed = out["addons"].as_array().expect("an array");
    assert_eq!(names(&out["addons"], "name"), ["bare", "sysinfo", "meter", "plain"]);

    let sysinfo = &listed[1];
    assert_eq!(sysinfo["kind"], "script");
    assert_eq!(sysinfo["label"], "SYSTEM INFO");
    assert_eq!(sysinfo["category"], "appgrid");
    assert_eq!(sysinfo["ref_h"], "4.5");
    assert_eq!(sysinfo["min_h"], Value::Null);

    // An addon that declares nothing declares nothing: the defaults it
    // will be given belong to the desktop, not to this listing.
    assert_eq!(listed[0]["label"], Value::Null);
    assert_eq!(listed[0]["category"], Value::Null);

    let meter = &listed[2];
    assert_eq!(meter["kind"], "plugin");
    assert_eq!(meter["label"], "METER");
    assert_eq!(meter["min_h"], "3.5");
    assert_eq!(listed[3]["label"], Value::Null, "no .meta, no declarations");
}

/// A pragma below the header is a comment about the code, not a
/// declaration — the same cut-off the desktop's own scan makes.
#[test]
fn a_pragma_past_the_header_is_not_a_declaration() {
    let install = install("pragma");
    let mut body = String::new();
    for i in 0..20 {
        body.push_str(&format!("// filler {i}\n"));
    }
    body.push_str("// label: TOO LATE\n");
    install.data_file("addons/scripts/late.rhai", &body);

    let out = run(&install.toolbox(), TOOL_LIST_ADDONS, json!({}));
    assert_eq!(out["addons"][0]["label"], Value::Null);
}

// ---- configuration -------------------------------------------------

#[test]
fn the_configuration_is_read_with_the_file_each_value_came_from() {
    let install = install("read-config");
    install.write_conf("# comment\nTheme=crimson\nSoundVolume=40\nMystery=42\n");

    let out = run(&install.toolbox(), TOOL_READ_CONFIG, json!({}));
    assert_eq!(out["user_file"], install.conf().display().to_string());
    let settings = out["settings"].as_array().expect("an array");
    assert_eq!(names(&out["settings"], "key"), ["Mystery", "SoundVolume", "Theme"]);
    for s in settings {
        assert_eq!(s["from"], install.conf().display().to_string());
    }
    assert_eq!(settings[0]["known"], false, "Mystery is not a key we write");
    assert_eq!(settings[2]["known"], true);

    let keys = names(&out["keys"], "key");
    for expected in ["Theme", "Layaut", "SoundVolume", "GridCols"] {
        assert!(keys.contains(&expected.to_string()), "{expected} must be offered");
    }
}

#[test]
fn one_key_is_set_and_the_previous_value_reported() {
    let install = install("set-config");
    install.write_conf("SoundVolume=40\n");
    let out = run(
        &install.toolbox(),
        TOOL_SET_CONFIG,
        json!({ "key": "SoundVolume", "value": "75" }),
    );
    assert_eq!(out["previous"], "40");
    assert_eq!(out["value"], "75");
    assert_eq!(out["backup"], format!("{}.bak", install.conf().display()));
    assert_eq!(
        conf_value(&install.conf(), "SoundVolume").as_deref(),
        Some("75")
    );
}

/// A model that sends `75` for a volume or `true` for a switch is being
/// reasonable; the file is text either way.
#[test]
fn a_number_or_a_boolean_argument_is_accepted() {
    let install = install("scalars");
    let tools = install.toolbox();
    run(
        &tools,
        TOOL_SET_CONFIG,
        json!({ "key": "SoundVolume", "value": 75 }),
    );
    run(
        &tools,
        TOOL_SET_CONFIG,
        json!({ "key": "SoundTyping", "value": false }),
    );
    assert_eq!(
        conf_value(&install.conf(), "SoundVolume").as_deref(),
        Some("75")
    );
    assert_eq!(
        conf_value(&install.conf(), "SoundTyping").as_deref(),
        Some("0")
    );
}

#[test]
fn a_key_the_desktop_does_not_read_is_refused_with_the_list_of_ones_it_does() {
    let install = install("unknown-key");
    let err = install
        .toolbox()
        .run(TOOL_SET_CONFIG, &json!({ "key": "Colour", "value": "red" }))
        .expect_err("must fail");
    match &err {
        ToolError::Rejected { reason } => {
            assert!(reason.contains("ColorSpace"), "must name the real keys: {reason}")
        }
        other => panic!("{other:?}"),
    }
    assert!(!install.conf().exists(), "a refusal creates no file");
}

#[test]
fn a_value_outside_the_range_the_desktop_honours_is_refused() {
    let install = install("range");
    install.write_conf("GridCols=30\n");
    for (key, value) in [
        ("GridCols", "400"),
        ("GridCols", "3"),
        ("SoundVolume", "yes"),
        ("SoundTyping", "2"),
        ("ColorDepth", "9"),
        ("ColorSpace", "teal"),
        ("ColorLut", "../evil.cube"),
    ] {
        let err = install
            .toolbox()
            .run(TOOL_SET_CONFIG, &json!({ "key": key, "value": value }))
            .expect_err("must be refused");
        assert!(
            matches!(err, ToolError::Rejected { .. }),
            "{key}={value} gave {err:?}"
        );
    }
    assert_eq!(
        fs::read_to_string(install.conf()).expect("read back"),
        "GridCols=30\n",
        "nothing may be written by a refused call"
    );
}

/// The desktop lower-cases these before matching, so a capitalised
/// answer is right and is stored the way the desktop will read it.
#[test]
fn a_word_value_is_stored_the_way_the_desktop_reads_it() {
    let install = install("word");
    run(
        &install.toolbox(),
        TOOL_SET_CONFIG,
        json!({ "key": "ColorSpace", "value": "Display P3" }),
    );
    assert_eq!(
        conf_value(&install.conf(), "ColorSpace").as_deref(),
        Some("display p3")
    );
}

// ---- the search path -----------------------------------------------

#[test]
fn the_users_file_wins_key_by_key_over_the_system_one() {
    let root = scratch("cascade");
    let user = root.join("config");
    let system = root.join("etc");
    fs::create_dir_all(user.join(APP)).expect("user config");
    fs::create_dir_all(system.join(APP)).expect("system config");
    fs::write(
        system.join(APP).join(CONF),
        "Theme=lockdown\nSoundVolume=20\n",
    )
    .expect("system conf");
    fs::write(user.join(APP).join(CONF), "Theme=crimson\n").expect("user conf");

    let env: HashMap<String, String> = [
        ("XDG_CONFIG_HOME", user.display().to_string()),
        ("XDG_CONFIG_DIRS", system.display().to_string()),
        ("XDG_DATA_HOME", root.join("data").display().to_string()),
        ("XDG_DATA_DIRS", root.join("share").display().to_string()),
        ("NACELLE_THEME_DIR", root.join("themes").display().to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();

    let out = run(&Toolbox::from_env(&env), TOOL_READ_CONFIG, json!({}));
    let settings = out["settings"].as_array().expect("an array");
    let theme = settings.iter().find(|s| s["key"] == "Theme").expect("Theme");
    let volume = settings
        .iter()
        .find(|s| s["key"] == "SoundVolume")
        .expect("SoundVolume");
    assert_eq!(theme["value"], "crimson");
    assert_eq!(theme["from"], user.join(APP).join(CONF).display().to_string());
    assert_eq!(volume["value"], "20", "a key the user never set is inherited");
    assert_eq!(
        volume["from"],
        system.join(APP).join(CONF).display().to_string()
    );
}

#[test]
fn the_first_data_root_holding_a_name_wins() {
    let root = scratch("shadow");
    let user = root.join("data");
    let system = root.join("share");
    for (base, body) in [(&user, "MINE"), (&system, "THEIRS")] {
        let dir = base.join(APP).join("layauts");
        fs::create_dir_all(&dir).expect("layauts");
        fs::write(dir.join("wide.layaut"), body).expect("layaut");
    }
    fs::write(
        system.join(APP).join("layauts").join("only-system.layaut"),
        "SYSTEM",
    )
    .expect("layaut");

    let env: HashMap<String, String> = [
        ("XDG_CONFIG_HOME", root.join("config").display().to_string()),
        ("XDG_CONFIG_DIRS", root.join("etc").display().to_string()),
        ("XDG_DATA_HOME", user.display().to_string()),
        ("XDG_DATA_DIRS", system.display().to_string()),
        ("NACELLE_THEME_DIR", root.join("themes").display().to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let tools = Toolbox::from_env(&env);

    let listed = run(&tools, TOOL_LIST_LAYAUTS, json!({}));
    assert_eq!(
        names(&listed["installed"], "name"),
        ["default", "only-system", "wide"]
    );
    let read = run(&tools, TOOL_READ_LAYAUT, json!({ "name": "wide" }));
    assert_eq!(read["text"], "MINE", "the user's copy shadows the system one");
}

/// Neither `XDG_CONFIG_HOME` nor `HOME`: there is nowhere legitimate to
/// write, and guessing — the desktop's own fallback is the current
/// directory — would be worse than refusing.
#[test]
fn with_no_home_at_all_a_write_is_refused_and_a_read_still_answers() {
    let empty: HashMap<String, String> = HashMap::new();
    let tools = Toolbox::from_env(&empty);
    let err = tools
        .run(TOOL_SET_THEME, &json!({ "name": "aurora" }))
        .expect_err("must fail");
    assert_eq!(err, ToolError::NoConfigDir);
    assert!(err.to_string().contains("XDG_CONFIG_HOME"));

    let out = run(&tools, TOOL_READ_CONFIG, json!({}));
    assert_eq!(out["user_file"], Value::Null);
}

// ---- calling ------------------------------------------------------

#[test]
fn an_unknown_tool_is_an_error_naming_itself() {
    let tools = install("unknown").toolbox();
    let err = tools.run("nacelle_launch_missiles", &json!({})).expect_err("no");
    assert_eq!(
        err,
        ToolError::UnknownTool {
            name: "nacelle_launch_missiles".into()
        }
    );
}

#[test]
fn arguments_of_the_wrong_shape_say_which_field_is_wrong() {
    let tools = install("args").toolbox();
    for (input, want) in [
        (json!({}), "\"name\" is required"),
        (json!({ "name": 7 }), "\"name\" must be a string"),
        (json!("wide"), "must be a JSON object"),
    ] {
        let err = tools.run(TOOL_READ_LAYAUT, &input).expect_err("must fail");
        assert!(err.to_string().contains(want), "{input} gave {err}");
    }
}

/// A listing tool takes no arguments, and a model that sends `null`
/// instead of `{}` means the same thing.
#[test]
fn a_listing_tool_accepts_null_arguments() {
    let tools = install("null-args").toolbox();
    tools.run(TOOL_LIST_THEMES, &Value::Null).expect("null is {}");
}

#[test]
fn a_call_becomes_a_tool_result_and_a_failure_is_marked_as_one() {
    let install = install("result");
    install.data_file("themes/aurora.theme", "[meta]\n");
    let tools = install.toolbox();

    let good = tools.result_for(&ToolCall {
        id: "call_1".into(),
        name: TOOL_LIST_THEMES.into(),
        input: json!({}),
    });
    match good {
        Content::ToolResult { id, output, is_error } => {
            assert_eq!(id, "call_1");
            assert!(!is_error);
            assert!(output.contains("aurora"));
        }
        other => panic!("{other:?}"),
    }

    let bad = tools.result_for(&ToolCall {
        id: "call_2".into(),
        name: TOOL_READ_LAYAUT.into(),
        input: json!({ "name": "nope" }),
    });
    match bad {
        Content::ToolResult { id, output, is_error } => {
            assert_eq!(id, "call_2");
            assert!(is_error, "the model has to be told it failed");
            assert!(output.contains("not installed"), "{output}");
        }
        other => panic!("{other:?}"),
    }
}
