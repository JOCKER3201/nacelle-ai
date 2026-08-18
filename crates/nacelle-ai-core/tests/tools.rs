//! Every tool, on a throwaway installation.
//!
//! Each test builds a whole nacelle directory tree in a temporary
//! directory and points a [`Toolbox`] at it. Nothing reads the process
//! environment, nothing touches the developer's own `~/.config`, and no
//! test depends on what happens to be installed on the machine running
//! it — which matters here more than usual, since the machine running
//! these tests may well have a real nacelle desktop on it.
//!
//! The directories are handed over explicitly rather than through
//! `XDG_*` for the same reason: `DesktopDirs::from_env` adds the system
//! search path, and `/usr/share/nacelle` existing or not would decide
//! whether a listing test passed. The tests that are ABOUT the search
//! path build their own environment map, with every system directory
//! pointed at a temporary one.

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

/// The family's folder, under every XDG root.
const APP: &str = "nacelle";
/// What that folder was called before. Read, never written.
const LEGACY_APP: &str = "nacelle-desktop";
/// The configuration file is named after the PROGRAM, so it did not
/// change with the folder.
const CONF: &str = "nacelle-desktop.conf";
/// Its RON successor — read first, and the only one ever written.
const CONF_RON: &str = "nacelle-desktop.ron";

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

/// One installation: `<root>/config/nacelle` and `<root>/data/nacelle`,
/// both created.
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

    /// The RON document — the only file a write may produce.
    fn conf_ron(&self) -> PathBuf {
        self.config.join(CONF_RON)
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

/// What a written RON document says about one key, read back through
/// the same parser the tools use — `None` when it says nothing and the
/// cascade would answer.
fn ron_value(path: &Path, key: &str) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let doc = nacelle_ai::tools::conf::parse_ron(&text)
        .unwrap_or_else(|e| panic!("{} does not parse: {e}", path.display()));
    nacelle_ai::tools::conf::field_value(&doc, key)
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
    let note = out["note"].as_str().expect("a note");
    assert!(
        note.contains("compiled"),
        "the listing must admit it cannot see the theme the toolkit carries"
    );
    assert!(
        note.contains("ONE") && note.contains("`default`"),
        "and it must say there is exactly one of them, by name: a note that says \
         `themes` compiled in, plural and unnamed, makes every misspelling a \
         plausible built-in — {note}"
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

    // The write goes to the RON document, SEEDED from the old file so
    // nothing the user had set is lost — and the old file itself stays
    // byte for byte as it was.
    assert_eq!(
        ron_value(&install.conf_ron(), "Theme").as_deref(),
        Some("aurora")
    );
    assert_eq!(
        ron_value(&install.conf_ron(), "SoundVolume").as_deref(),
        Some("40"),
        "the first write carries the old file's other settings across"
    );
    assert_eq!(
        fs::read_to_string(install.conf()).expect("old file"),
        "# mine\nSoundVolume=40\nTheme=crimson\n",
        "the Key=Value file is read, never written"
    );
}

/// A name with no file behind it is a name the desktop will not draw:
/// `default` is the ONE theme compiled into the toolkit, so `lockdown`
/// (a variant that left on 2026-08-16) resolves to nothing. The file is
/// the user's, so this is a warning attached to a completed write and
/// not a refusal — but the warning has to say the name is wrong, and
/// which name would have been right.
#[test]
fn choosing_a_theme_with_no_file_warns_but_still_writes() {
    let install = install("set-theme-builtin");
    let out = run(
        &install.toolbox(),
        TOOL_SET_THEME,
        json!({ "name": "lockdown" }),
    );
    assert_eq!(out["value"], "lockdown");
    let warning = out["warning"].as_str().expect("a warning").to_string();
    assert!(warning.contains("compiled into the toolkit"), "{out}");
    assert!(
        warning.contains("`default`"),
        "the warning must NAME the one built-in, or a model reading it still \
         cannot tell a typo from a theme that exists: {out}"
    );
    assert_eq!(
        ron_value(&install.conf_ron(), "Theme").as_deref(),
        Some("lockdown")
    );
}

/// And the one name that is not a file and is still right draws no
/// warning at all. `default` is the master compiled into the toolkit; a
/// tool that warned about it would be telling the user their working
/// choice had failed.
#[test]
fn choosing_the_built_in_theme_is_not_warned_about() {
    let install = install("set-theme-default");
    let out = run(
        &install.toolbox(),
        TOOL_SET_THEME,
        json!({ "name": "default" }),
    );
    assert_eq!(out["value"], "default");
    assert!(
        out.get("warning").is_none(),
        "`default` is the theme the toolkit compiles in — there is nothing to warn \
         about: {out}"
    );
    assert_eq!(
        ron_value(&install.conf_ron(), "Theme").as_deref(),
        Some("default")
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
        ron_value(&install.conf_ron(), "Layaut").as_deref(),
        Some("narrow")
    );
}

/// Clearing is a real operation: an empty name REMOVES the field, so
/// the rest of the cascade answers again — and the user's old file,
/// which still says `Layaut=wide`, is outranked by the new document
/// standing beside it.
#[test]
fn an_empty_name_clears_the_layaut_setting() {
    let install = install("clear-layaut");
    install.write_conf("Layaut=wide\n");
    let out = run(&install.toolbox(), TOOL_SET_LAYAUT, json!({ "name": "" }));
    assert_eq!(out["value"], "");
    assert!(install.conf_ron().is_file(), "the cleared document is written");
    assert_eq!(
        ron_value(&install.conf_ron(), "Layaut"),
        None,
        "cleared means the field is gone, not an empty name"
    );
    assert_eq!(
        conf_value(&install.conf(), "Layaut").as_deref(),
        Some("wide"),
        "the old file is read, never written"
    );
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
    assert_eq!(
        out["user_file"],
        install.conf_ron().display().to_string(),
        "the file a tool may write is the RON document"
    );
    let settings = out["settings"].as_array().expect("an array");
    // `Mystery` is not a setting the desktop reads, so the typed
    // document has nowhere to put it and the report does not invent a
    // place. The values that ARE settings come from the old file, and
    // the report says so.
    assert_eq!(names(&out["settings"], "key"), ["SoundVolume", "Theme"]);
    for s in settings {
        assert_eq!(s["from"], install.conf().display().to_string());
    }

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
    assert_eq!(out["previous"], "40", "the old file's value is the previous one");
    assert_eq!(out["value"], "75");
    assert_eq!(
        out["backup"],
        Value::Null,
        "the first write creates the RON document; there is nothing of it to back up"
    );
    assert_eq!(
        ron_value(&install.conf_ron(), "SoundVolume").as_deref(),
        Some("75")
    );

    // The second write replaces the document this program wrote, and
    // THAT is backed up beside it.
    let out = run(
        &install.toolbox(),
        TOOL_SET_CONFIG,
        json!({ "key": "SoundVolume", "value": "60" }),
    );
    assert_eq!(out["previous"], "75");
    assert_eq!(
        out["backup"],
        format!("{}.bak", install.conf_ron().display())
    );
    assert_eq!(
        conf_value(&install.conf(), "SoundVolume").as_deref(),
        Some("40"),
        "the old file keeps saying what it always said"
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
        ron_value(&install.conf_ron(), "SoundVolume").as_deref(),
        Some("75")
    );
    assert_eq!(
        ron_value(&install.conf_ron(), "SoundTyping").as_deref(),
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
        ron_value(&install.conf_ron(), "ColorSpace").as_deref(),
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

/// An environment with every system directory pointed at a temporary
/// one, so a test says nothing about the machine it runs on.
fn env_at(root: &Path) -> HashMap<String, String> {
    [
        ("XDG_CONFIG_HOME", root.join("config").display().to_string()),
        ("XDG_CONFIG_DIRS", root.join("etc").display().to_string()),
        ("XDG_DATA_HOME", root.join("data").display().to_string()),
        ("XDG_DATA_DIRS", root.join("share").display().to_string()),
        ("NACELLE_THEME_DIR", root.join("themes").display().to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

/// The folder used to be named after the desktop alone. A machine that
/// has one is READ, never moved: this is the case that covers every
/// installation made before the rename.
#[test]
fn a_configuration_under_the_folders_old_name_is_still_read() {
    let root = scratch("legacy-conf");
    let old = root.join("config").join(LEGACY_APP);
    fs::create_dir_all(&old).expect("old config directory");
    fs::write(old.join(CONF), "Theme=crimson\nSoundVolume=20\n").expect("conf");

    let out = run(&Toolbox::from_env(&env_at(&root)), TOOL_READ_CONFIG, json!({}));
    let settings = out["settings"].as_array().expect("an array");
    let theme = settings.iter().find(|s| s["key"] == "Theme").expect("Theme");
    assert_eq!(theme["value"], "crimson");
    assert_eq!(
        theme["from"],
        old.join(CONF).display().to_string(),
        "the value must be reported from the file it really came from"
    );
}

/// With both folders in place the new one wins key by key, and a key
/// only the old file carries is still inherited — one cascade, the two
/// names one rung apart.
#[test]
fn the_new_folder_wins_over_the_old_one_and_both_over_the_system() {
    let root = scratch("both-conf");
    let new = root.join("config").join(APP);
    let old = root.join("config").join(LEGACY_APP);
    let system = root.join("etc").join(APP);
    for dir in [&new, &old, &system] {
        fs::create_dir_all(dir).expect("directory");
    }
    fs::write(system.join(CONF), "Theme=lockdown\nLayaut=lockdown\nSoundVolume=5\n")
        .expect("system conf");
    fs::write(old.join(CONF), "Theme=crimson\nLayaut=console\n").expect("old conf");
    fs::write(new.join(CONF), "Theme=azure\n").expect("new conf");

    let out = run(&Toolbox::from_env(&env_at(&root)), TOOL_READ_CONFIG, json!({}));
    let settings = out["settings"].as_array().expect("an array");
    let value = |key: &str| {
        settings
            .iter()
            .find(|s| s["key"] == key)
            .unwrap_or_else(|| panic!("{key} must be answered"))
            .clone()
    };
    assert_eq!(value("Theme")["value"], "azure", "the new folder wins");
    assert_eq!(
        value("Layaut")["value"],
        "console",
        "the user's OLD file still outranks the system defaults"
    );
    assert_eq!(
        value("SoundVolume")["value"],
        "5",
        "a key neither user file has still comes from the system one"
    );
    assert_eq!(
        out["user_file"],
        new.join(CONF_RON).display().to_string(),
        "the file a tool may write is the new folder's RON document, whatever exists beside it"
    );
}

/// The data tree keeps both names too: a layaut installed before the
/// rename is still listed and still readable, and one under the new
/// name shadows it exactly as a user install shadows a system one.
#[test]
fn a_layaut_installed_under_the_old_name_is_still_found() {
    let root = scratch("legacy-data");
    let old = root.join("data").join(LEGACY_APP).join("layauts");
    fs::create_dir_all(&old).expect("old layauts");
    fs::write(old.join("wide.layaut"), "OLD").expect("layaut");
    fs::write(old.join("only-old.layaut"), "ONLY OLD").expect("layaut");

    let tools = Toolbox::from_env(&env_at(&root));
    assert_eq!(
        names(&run(&tools, TOOL_LIST_LAYAUTS, json!({}))["installed"], "name"),
        ["default", "only-old", "wide"]
    );
    assert_eq!(
        run(&tools, TOOL_READ_LAYAUT, json!({ "name": "wide" }))["text"],
        "OLD"
    );

    // The same name under the new folder takes over, and is listed once.
    let new = root.join("data").join(APP).join("layauts");
    fs::create_dir_all(&new).expect("new layauts");
    fs::write(new.join("wide.layaut"), "NEW").expect("layaut");
    let tools = Toolbox::from_env(&env_at(&root));
    assert_eq!(
        names(&run(&tools, TOOL_LIST_LAYAUTS, json!({}))["installed"], "name"),
        ["default", "only-old", "wide"],
        "one layaut, listed once"
    );
    assert_eq!(
        run(&tools, TOOL_READ_LAYAUT, json!({ "name": "wide" }))["text"],
        "NEW",
        "the new folder wins when both hold the same name"
    );
}

/// Writing goes to the new folder and only there. The user's old file
/// is left byte for byte as it was found — which is what makes the
/// rename reversible.
#[test]
fn a_write_lands_in_the_new_folder_and_the_old_file_is_untouched() {
    let root = scratch("legacy-write");
    let old = root.join("config").join(LEGACY_APP);
    fs::create_dir_all(&old).expect("old config directory");
    let before = "# somebody's own file\nTheme=crimson\nSounds=classic\n";
    fs::write(old.join(CONF), before).expect("conf");

    let tools = Toolbox::from_env(&env_at(&root));
    run(&tools, TOOL_SET_THEME, json!({ "name": "azure" }));

    let new = root.join("config").join(APP).join(CONF_RON);
    assert!(new.is_file(), "the write must land in the new folder");
    assert_eq!(ron_value(&new, "Theme").as_deref(), Some("azure"));
    assert_eq!(
        ron_value(&new, "Sounds").as_deref(),
        Some("classic"),
        "the new document is seeded from the old folder's file, so nothing is lost"
    );
    assert_eq!(
        fs::read_to_string(old.join(CONF)).expect("old conf"),
        before,
        "the user's old file may not be touched, moved or rewritten"
    );
}

/// What the runnable program prints its one line from. A machine with
/// no old folder is told nothing at all.
#[test]
fn the_old_folders_that_are_really_there_are_the_ones_reported() {
    let root = scratch("legacy-report");
    fs::create_dir_all(root.join("config").join(APP)).expect("new config");
    let dirs = DesktopDirs::from_env(&env_at(&root));
    assert!(
        dirs.legacy_dirs_in_use().is_empty(),
        "a machine that never had the old folder must be told nothing"
    );

    let old_conf = root.join("config").join(LEGACY_APP);
    let old_data = root.join("data").join(LEGACY_APP);
    fs::create_dir_all(&old_conf).expect("old config");
    fs::create_dir_all(&old_data).expect("old data");
    let dirs = DesktopDirs::from_env(&env_at(&root));
    assert_eq!(
        dirs.legacy_dirs_in_use(),
        vec![old_conf, old_data],
        "both trees are reported, configuration first, and each one once"
    );
}

/// The ordinary machine's shape: no `XDG_*` set at all, just `HOME`.
/// Both folder names stand under `~/.config`, the new one first, and
/// the system end of the cascade carries the pair as well.
#[test]
fn with_only_home_set_both_folder_names_stand_under_dot_config() {
    let root = scratch("home-only");
    let env: HashMap<String, String> = [("HOME".to_string(), root.display().to_string())]
        .into_iter()
        .collect();
    let dirs = DesktopDirs::from_env(&env);
    let config = root.join(".config");
    let levels = dirs.conf_levels();
    assert_eq!(
        levels.iter().map(|l| l.dir.clone()).collect::<Vec<_>>(),
        vec![
            config.join(APP),
            config.join(LEGACY_APP),
            PathBuf::from("/etc/xdg").join(APP),
            PathBuf::from("/etc/xdg").join(LEGACY_APP),
        ]
    );
    for level in &levels {
        assert_eq!(
            level.ron,
            level.dir.join(CONF_RON),
            "every rung reads the RON document first"
        );
        assert_eq!(
            level.legacy,
            level.dir.join(CONF),
            "and falls back to the old Key=Value file beside it"
        );
    }
    assert_eq!(
        dirs.user_conf_levels()
            .iter()
            .map(|l| l.dir.clone())
            .collect::<Vec<_>>(),
        vec![config.join(APP), config.join(LEGACY_APP)],
        "only the user's own rungs may seed a write"
    );
    assert_eq!(
        dirs.config_dir().expect("HOME is set"),
        config.join(APP),
        "the write target is the family folder, never the old name"
    );
    assert_eq!(
        dirs.data_roots()[..2],
        [
            root.join(".local/share").join(APP),
            root.join(".local/share").join(LEGACY_APP),
        ],
        "the data tree pairs the names the same way"
    );
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
