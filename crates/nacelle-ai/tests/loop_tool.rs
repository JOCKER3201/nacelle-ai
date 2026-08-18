//! The `loop` tool against a counterfeit ffmpeg: a shell script that
//! counts its invocations and logs its arguments, so the tests can
//! verify WHAT would have been executed without executing anything —
//! and can prove the two rules the tool exists to keep: ffmpeg is only
//! ever run (never needed for parsing or planning), and the result is a
//! NEW file beside the source with the input untouched.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use nacelle_ai_daemon::media::{run_loop, Course, Ffmpeg, Outcome};
use serde_json::json;

/// One stage at a time, deliberately. These tests write a script and
/// then exec it, and a `fork` on another test's thread can inherit the
/// script's still-open descriptor for the instant before its `exec`
/// closes it — at which moment running the script fails with "text
/// file busy". Rare, real, and gone entirely once no write and no
/// spawn overlap.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// A course nobody steers: progress vanishes, and `stops` says whether
/// the user has "pressed stop" before every step boundary.
struct Quiet {
    stops: bool,
}

impl Course for Quiet {
    fn progress(&mut self, _msg: &str) {}

    fn stopped(&mut self) -> bool {
        self.stops
    }
}

/// A private stage for one test, with the counterfeit ffmpeg on it.
struct Stage {
    dir: PathBuf,
    ffmpeg: Ffmpeg,
    log: PathBuf,
    _turn: MutexGuard<'static, ()>,
}

impl Stage {
    fn build(name: &str) -> Stage {
        let turn = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir =
            std::env::temp_dir().join(format!("nacelle-ai-loop-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("calls.log");

        // The counterfeit: every call appends its arguments to the log.
        // A call carrying a filter (`-filter_complex` or `-vf`) is a
        // RENDER — it creates its last argument, as ffmpeg would, and
        // succeeds. Anything else is the PROBE (`ffmpeg -i` with no
        // output), which prints a Duration banner on stderr and exits
        // nonzero, exactly as the real one does.
        let script = dir.join("ffmpeg");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\n\
                 echo \"$@\" >> \"{log}\"\n\
                 case \" $* \" in\n\
                 *\" -filter_complex \"*|*\" -vf \"*)\n\
                     for a in \"$@\"; do last=\"$a\"; done\n\
                     : > \"$last\"\n\
                     exit 0\n\
                     ;;\n\
                 *)\n\
                     echo '  Duration: 00:00:10.00, start: 0.000000, bitrate: 1 kb/s' >&2\n\
                     exit 1\n\
                     ;;\n\
                 esac\n",
                log = log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

        Stage {
            ffmpeg: Ffmpeg::at(&script),
            dir,
            log,
            _turn: turn,
        }
    }

    fn file(&self, name: &str, content: &str) -> PathBuf {
        let path = self.dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    /// Every recorded invocation, one line of arguments each.
    fn calls(&self) -> Vec<String> {
        match fs::read_to_string(&self.log) {
            Ok(text) => text.lines().map(str::to_string).collect(),
            Err(_) => Vec::new(),
        }
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn done(outcome: Result<Outcome, String>) -> PathBuf {
    match outcome {
        Ok(Outcome::Done(path)) => path,
        other => panic!("expected a finished loop, got {other:?}"),
    }
}

#[test]
fn a_video_becomes_a_new_loop_beside_it() {
    let stage = Stage::build("video");
    let input = stage.file("clip.mp4", "pretend video bytes");

    let out = done(run_loop(
        &stage.ffmpeg,
        &json!({ "path": input.display().to_string() }),
        &mut Quiet { stops: false },
    ));

    assert_eq!(out, stage.dir.join("clip-loop.mp4"), "a NEW file, beside the source");
    assert!(out.is_file());
    assert_eq!(
        fs::read_to_string(&input).unwrap(),
        "pretend video bytes",
        "the input is untouched"
    );

    let calls = stage.calls();
    assert_eq!(calls.len(), 2, "one probe, one render: {calls:?}");
    assert!(calls[0].contains("-i"), "the probe reads the input");
    assert!(!calls[0].contains("clip-loop"), "the probe writes nothing");
    assert!(calls[1].contains("xfade"), "the render cross-fades the clip into itself");
    assert!(calls[1].contains("-n"), "ffmpeg itself refuses to overwrite");
    assert!(calls[1].contains("-an"), "v0 drops audio rather than faking a seamless splice");
}

#[test]
fn one_photo_becomes_a_minute_on_repeat() {
    let stage = Stage::build("photo");
    let input = stage.file("shot.jpg", "pretend jpeg");

    let out = done(run_loop(
        &stage.ffmpeg,
        &json!({ "path": input.display().to_string() }),
        &mut Quiet { stops: false },
    ));

    assert_eq!(out, stage.dir.join("shot-loop.mp4"));
    let calls = stage.calls();
    assert_eq!(calls.len(), 1, "a photo needs no probe: {calls:?}");
    assert!(calls[0].contains("-loop 1"), "the still is looped as input");
    assert!(calls[0].contains("-t 60"), "the owner asked for one minute");
}

#[test]
fn several_photos_cycle_through_the_minute() {
    let stage = Stage::build("photos");
    let first = stage.file("a.png", "a");
    let second = stage.file("b.png", "b");

    let out = done(run_loop(
        &stage.ffmpeg,
        &json!({ "paths": [first.display().to_string(), second.display().to_string()] }),
        &mut Quiet { stops: false },
    ));

    assert_eq!(out, stage.dir.join("a-loop.mp4"), "beside the FIRST photo");
    let calls = stage.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].contains("concat"), "several stills ride the concat demuxer");
    assert!(calls[0].contains("-t 60"));
}

#[test]
fn a_directory_stands_for_the_photos_inside_it() {
    let stage = Stage::build("dir");
    stage.file("b.png", "b");
    stage.file("a.png", "a");
    stage.file("notes.txt", "not a photo, passed over");

    let out = done(run_loop(
        &stage.ffmpeg,
        &json!({ "path": stage.dir.display().to_string() }),
        &mut Quiet { stops: false },
    ));

    // Name order decides who is first, so the result sits beside a.png.
    assert_eq!(out, stage.dir.join("a-loop.mp4"), "beside the FIRST photo by name");
    let calls = stage.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].contains("concat"), "the directory's stills ride the concat demuxer");
    assert!(calls[0].contains("-t 60"));
}

#[test]
fn a_directory_with_no_photos_is_refused() {
    let stage = Stage::build("dir-empty");
    stage.file("notes.txt", "prose");

    let err = run_loop(
        &stage.ffmpeg,
        &json!({ "path": stage.dir.display().to_string() }),
        &mut Quiet { stops: false },
    )
    .unwrap_err();

    assert!(err.contains("holds no photos"), "{err}");
    assert_eq!(stage.calls().len(), 0, "nothing to render, nothing ran");
}

#[test]
fn the_output_never_overwrites_an_earlier_loop() {
    let stage = Stage::build("counted");
    let input = stage.file("clip.mp4", "video");
    stage.file("clip-loop.mp4", "an earlier run's result");

    let out = done(run_loop(
        &stage.ffmpeg,
        &json!({ "path": input.display().to_string() }),
        &mut Quiet { stops: false },
    ));

    assert_eq!(out, stage.dir.join("clip-loop-2.mp4"), "counted up, not replaced");
    assert_eq!(
        fs::read_to_string(stage.dir.join("clip-loop.mp4")).unwrap(),
        "an earlier run's result"
    );
}

#[test]
fn a_stop_between_steps_cancels_with_nothing_run() {
    let stage = Stage::build("stopped");
    let photo = stage.file("shot.png", "png");

    let outcome = run_loop(
        &stage.ffmpeg,
        &json!({ "path": photo.display().to_string() }),
        &mut Quiet { stops: true },
    );
    assert_eq!(outcome, Ok(Outcome::Cancelled));
    assert_eq!(stage.calls().len(), 0, "a stopped photo run never execs at all");

    // A video probes first — the stop lands at the next boundary, and
    // the render never happens.
    let video = stage.file("clip.mp4", "video");
    let outcome = run_loop(
        &stage.ffmpeg,
        &json!({ "path": video.display().to_string() }),
        &mut Quiet { stops: true },
    );
    assert_eq!(outcome, Ok(Outcome::Cancelled));
    let calls = stage.calls();
    assert_eq!(calls.len(), 1, "the probe ran, the render did not: {calls:?}");
    assert!(!stage.dir.join("clip-loop.mp4").exists());
}

#[test]
fn missing_ffmpeg_is_a_sentence_that_says_what_to_do() {
    // An empty PATH and no override: nothing to find.
    let empty = std::env::temp_dir().join(format!("nacelle-ai-nopath-{}", std::process::id()));
    let _ = fs::create_dir_all(&empty);
    let err = Ffmpeg::find(&|name| match name {
        "PATH" => Some(empty.display().to_string()),
        _ => None,
    })
    .expect_err("there is no ffmpeg on an empty PATH");
    assert!(err.contains("not installed"), "said: {err}");
    assert!(err.contains("install"), "it says what to do: {err}");
    let _ = fs::remove_dir_all(&empty);
}

#[test]
fn an_override_that_is_not_executable_is_named_in_the_error() {
    let stage = Stage::build("override");
    let plain = stage.file("not-ffmpeg", "just text");
    fs::set_permissions(&plain, fs::Permissions::from_mode(0o644)).unwrap();

    let err = Ffmpeg::find(&|name| match name {
        "NACELLE_AI_FFMPEG" => Some(plain.display().to_string()),
        _ => None,
    })
    .expect_err("a plain file is not an ffmpeg");
    assert!(err.contains("NACELLE_AI_FFMPEG"), "said: {err}");
}

/// The order the whole family keeps: the environment above the file.
/// `NACELLE_AI_FFMPEG` is what somebody exports for one run, and a line
/// written into `nacelle-ai.ron` months ago must not beat it.
#[test]
fn the_environment_outranks_the_file_when_both_name_an_ffmpeg() {
    let stage = Stage::build("pickorder");
    let other = stage.file("other-ffmpeg", "#!/bin/sh\nexit 0\n");
    fs::set_permissions(&other, fs::Permissions::from_mode(0o755)).unwrap();

    let exported = stage.dir.join("ffmpeg");

    // Both name one: the environment's answer is the one used.
    let picked = Ffmpeg::pick(
        &|name| match name {
            "NACELLE_AI_FFMPEG" => Some(exported.display().to_string()),
            _ => None,
        },
        Some(&other),
    )
    .expect("both are executable");
    assert!(
        format!("{picked:?}").contains(&exported.display().to_string()),
        "picked: {picked:?}"
    );

    // Only the file names one: the file's answer is used, without
    // PATH being consulted at all.
    let picked = Ffmpeg::pick(&|_| None, Some(&other)).expect("the file's ffmpeg is executable");
    assert!(
        format!("{picked:?}").contains(&other.display().to_string()),
        "picked: {picked:?}"
    );
}

/// A named program that is not an executable is an error, never a
/// quiet fall-through to whatever `PATH` happens to hold: somebody who
/// named an ffmpeg wants that one.
#[test]
fn an_ffmpeg_named_in_the_file_that_is_not_executable_is_an_error() {
    let stage = Stage::build("pickplain");
    let plain = stage.file("not-ffmpeg", "just text");
    fs::set_permissions(&plain, fs::Permissions::from_mode(0o644)).unwrap();

    let err = Ffmpeg::pick(&|_| None, Some(&plain)).expect_err("a plain file is not an ffmpeg");
    assert!(err.contains("nacelle-ai.ron"), "said: {err}");
    assert!(err.contains("not-ffmpeg"), "said: {err}");
}

#[test]
fn what_is_not_media_is_refused_before_anything_runs() {
    let stage = Stage::build("notmedia");
    let odd = stage.file("notes.txt", "text");

    let err = run_loop(
        &stage.ffmpeg,
        &json!({ "path": odd.display().to_string() }),
        &mut Quiet { stops: false },
    )
    .expect_err("a text file is not media");
    assert!(err.contains("notes.txt"), "said: {err}");
    assert_eq!(stage.calls().len(), 0, "refused without an exec");
}

#[test]
fn several_files_must_all_be_photos() {
    let stage = Stage::build("mixed");
    let photo = stage.file("a.png", "a");
    let video = stage.file("b.mp4", "b");

    let err = run_loop(
        &stage.ffmpeg,
        &json!({ "paths": [photo.display().to_string(), video.display().to_string()] }),
        &mut Quiet { stops: false },
    )
    .expect_err("a video among the photos");
    assert!(err.contains("b.mp4"), "said: {err}");
    assert_eq!(stage.calls().len(), 0);
}

#[test]
fn a_file_that_is_not_there_is_said_by_name() {
    let stage = Stage::build("missing");
    let ghost = stage.dir.join("ghost.mp4");
    let err = run_loop(
        &stage.ffmpeg,
        &json!({ "path": ghost.display().to_string() }),
        &mut Quiet { stops: false },
    )
    .expect_err("a missing file");
    assert!(err.contains("ghost.mp4"), "said: {err}");
    assert_eq!(stage.calls().len(), 0);
}

/// The empty and wrong argument shapes, refused with the field named.
#[test]
fn the_argument_shapes_are_policed() {
    let stage = Stage::build("args");
    let mut quiet = Quiet { stops: false };
    for (args, names) in [
        (json!({}), "path"),
        (json!({ "path": "" }), "path"),
        (json!({ "paths": [] }), "paths"),
        (json!({ "paths": "not-a-list" }), "paths"),
    ] {
        let err = run_loop(&stage.ffmpeg, &args, &mut quiet).expect_err("bad args");
        assert!(err.contains(names), "{args} → {err}");
    }
    assert_eq!(stage.calls().len(), 0);
}

/// Paths with spaces survive the argument list — exec takes an argv,
/// not a shell string, and the test would catch anyone changing that.
#[test]
fn a_path_with_spaces_survives() {
    let stage = Stage::build("spaces");
    let spaced_dir = stage.dir.join("my clips");
    fs::create_dir_all(&spaced_dir).unwrap();
    let input = spaced_dir.join("summer trip.mp4");
    fs::write(&input, "video").unwrap();

    let out = done(run_loop(
        &stage.ffmpeg,
        &json!({ "path": input.display().to_string() }),
        &mut Quiet { stops: false },
    ));
    assert_eq!(out, spaced_dir.join("summer trip-loop.mp4"));
    assert!(out.is_file());
}
