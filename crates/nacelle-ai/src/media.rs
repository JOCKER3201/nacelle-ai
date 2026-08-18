//! The `loop` tool: media in, a loop out, ffmpeg by exec.
//!
//! A VIDEO becomes a seamless loop: the clip's own opening seconds are
//! cross-faded over its ending, so the last frame the loop shows is the
//! frame it re-enters on. ONE OR MORE PHOTOS become a one-minute clip
//! that cycles through them, made to be played on repeat.
//!
//! Two rules from the licence page and the spec, both load-bearing:
//!
//! **ffmpeg is EXECUTED, never copied.** Running somebody else's
//! program is fine; copying or translating its code is forbidden.
//! Everything here builds argument lists and reads exit codes. When
//! ffmpeg is not installed, the answer is an error that says so and how
//! to fix it — nothing else is attempted.
//!
//! **The result is a NEW file beside the source.** Nothing overwrites
//! the input, and nothing overwrites anything else either: the output
//! name is `<stem>-loop.<ext>`, counted up (`-loop-2`, `-loop-3`, …)
//! until a free name is found, and ffmpeg itself is run with `-n` so a
//! file that appears in the gap between the check and the run fails the
//! run instead of being replaced.
//!
//! No model is involved anywhere in this module — the media tools are
//! deterministic, which is half of the daemon's local-model policy (the
//! local model manages the interface; it is never handed the user's
//! files). See `backends`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The environment variable that overrides where ffmpeg is. For a
/// machine with a private build — and for the tests, which point it at
/// a counting stand-in.
pub const FFMPEG_ENV: &str = "NACELLE_AI_FFMPEG";

/// What drives one run: where progress lines go, and whether the user
/// has since said stop. Implemented by the connection; a test implements
/// it with two fields.
pub trait Course {
    fn progress(&mut self, msg: &str);
    /// Checked between the steps of a run. A blocking ffmpeg child is
    /// not interrupted mid-encode in v0 — a stop takes effect at the
    /// next step boundary, and the daemon says what was left behind.
    fn stopped(&mut self) -> bool;
}

/// How a run ended, short of an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The new file, beside its source.
    Done(PathBuf),
    /// The user cancelled between steps. Nothing of the output remains.
    Cancelled,
}

/// The ffmpeg this tool will exec.
#[derive(Clone, Debug)]
pub struct Ffmpeg {
    program: PathBuf,
}

impl Ffmpeg {
    /// A specific binary, for a caller that already knows — the tests.
    pub fn at(program: impl Into<PathBuf>) -> Ffmpeg {
        Ffmpeg {
            program: program.into(),
        }
    }

    /// The machine's ffmpeg: [`FFMPEG_ENV`] when set, else the first
    /// executable `ffmpeg` on `PATH`. The error is the sentence the
    /// client shows, so it says what to do rather than what failed.
    pub fn find(env: &dyn Fn(&str) -> Option<String>) -> Result<Ffmpeg, String> {
        if let Some(named) = env(FFMPEG_ENV).filter(|v| !v.trim().is_empty()) {
            let program = PathBuf::from(named.trim());
            if is_executable(&program) {
                return Ok(Ffmpeg { program });
            }
            return Err(format!(
                "{} names {}, which is not an executable file",
                FFMPEG_ENV,
                program.display()
            ));
        }
        for dir in env("PATH").unwrap_or_default().split(':') {
            if dir.is_empty() {
                continue;
            }
            let candidate = Path::new(dir).join("ffmpeg");
            if is_executable(&candidate) {
                return Ok(Ffmpeg {
                    program: candidate,
                });
            }
        }
        Err("ffmpeg is not installed (not found on PATH) — the loop tool runs ffmpeg to \
             build the clip; install it and try again"
            .to_string())
    }

    /// The ffmpeg to use, given what the environment says and what the
    /// configuration file said.
    ///
    /// The order is the family's: the environment above the file, so a
    /// variable somebody exported for this run beats a line written
    /// down months ago; and `PATH` last, which is what happens when
    /// neither names one. A named program that is not an executable
    /// file is an ERROR rather than a fall-through to `PATH` — a person
    /// who named an ffmpeg wants that ffmpeg, and silently running a
    /// different one answers a different question.
    pub fn pick(
        env: &dyn Fn(&str) -> Option<String>,
        configured: Option<&Path>,
    ) -> Result<Ffmpeg, String> {
        if env(FFMPEG_ENV)
            .filter(|v| !v.trim().is_empty())
            .is_some()
        {
            return Ffmpeg::find(env);
        }
        let Some(named) = configured else {
            return Ffmpeg::find(env);
        };
        if is_executable(named) {
            return Ok(Ffmpeg {
                program: named.to_path_buf(),
            });
        }
        Err(format!(
            "nacelle-ai.ron names {} as ffmpeg, which is not an executable file",
            named.display()
        ))
    }

    fn run(&self, args: &[String]) -> Result<Output, String> {
        Command::new(&self.program)
            .args(args)
            .output()
            .map_err(|e| format!("could not run {}: {e}", self.program.display()))
    }
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

/// How long the photo clip runs: the owner asked for one minute.
const PHOTO_CLIP_SECONDS: f64 = 60.0;

/// The most a cross-fade is allowed to take. A quarter of a very short
/// clip, one second of anything longer.
const FADE_CAP_SECONDS: f64 = 1.0;

/// What a file's extension says it is.
enum Media {
    Video,
    Image,
}

fn media_of(path: &Path) -> Result<Media, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" | "mpg" | "mpeg" | "ts" | "gif" => {
            Ok(Media::Video)
        }
        "png" | "jpg" | "jpeg" | "webp" | "bmp" | "tif" | "tiff" | "avif" | "jxl" => {
            Ok(Media::Image)
        }
        _ => Err(format!(
            "{} is not a media file this tool knows — it takes a video (mp4, mkv, webm, \
             mov, avi, m4v, mpg, ts, gif) or photos (png, jpg, webp, bmp, tiff, avif, jxl)",
            path.display()
        )),
    }
}

/// The `loop` tool. `args` is the command's `args` object:
/// `{"path": "..."}` for one file, `{"paths": ["...", …]}` for several
/// photos. One video makes a seamless loop; photos make a one-minute
/// cycling clip.
pub fn run_loop(
    ffmpeg: &Ffmpeg,
    args: &serde_json::Value,
    course: &mut dyn Course,
) -> Result<Outcome, String> {
    let mut inputs = inputs_of(args)?;
    // The command's dictionary says `path` names a file OR a directory:
    // a directory stands for the photos inside it, in name order.
    if let [only] = inputs.as_slice() {
        if only.is_dir() {
            inputs = photos_inside(only)?;
        }
    }
    for input in &inputs {
        if !input.is_file() {
            return Err(format!("{} does not exist or is not a file", input.display()));
        }
    }
    match (inputs.len(), media_of(&inputs[0])?) {
        (1, Media::Video) => video_loop(ffmpeg, &inputs[0], course),
        (_, Media::Image) => {
            for input in &inputs {
                if !matches!(media_of(input)?, Media::Image) {
                    return Err(format!(
                        "{} is not a photo — several files at once must all be photos",
                        input.display()
                    ));
                }
            }
            photo_loop(ffmpeg, &inputs, course)
        }
        (_, Media::Video) => Err(
            "several files at once must all be photos; a video is looped one at a time"
                .to_string(),
        ),
    }
}

/// The photos a directory stands for: its image files, in name order.
/// Files of other kinds are passed over without complaint — a folder of
/// holiday photos may hold a stray text file — but an empty harvest is
/// an error, because the caller asked for a loop and nothing can loop.
fn photos_inside(dir: &std::path::Path) -> Result<Vec<PathBuf>, String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("{} cannot be read: {e}", dir.display()))?;
    let mut photos = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|e| format!("{} cannot be read: {e}", dir.display()))?
            .path();
        if path.is_file() && matches!(media_of(&path), Ok(Media::Image)) {
            photos.push(path);
        }
    }
    photos.sort();
    if photos.is_empty() {
        return Err(format!(
            "{} holds no photos this tool knows (png, jpg, webp, bmp, tiff, avif, jxl)",
            dir.display()
        ));
    }
    Ok(photos)
}

/// The absolute paths the command names, in its order.
fn inputs_of(args: &serde_json::Value) -> Result<Vec<PathBuf>, String> {
    if let Some(path) = args.get("path") {
        let Some(path) = path.as_str().filter(|p| !p.trim().is_empty()) else {
            return Err("\"path\" is a file path".to_string());
        };
        return Ok(vec![PathBuf::from(path)]);
    }
    if let Some(paths) = args.get("paths") {
        let Some(list) = paths.as_array() else {
            return Err("\"paths\" is a list of file paths".to_string());
        };
        let mut out = Vec::new();
        for entry in list {
            let Some(path) = entry.as_str().filter(|p| !p.trim().is_empty()) else {
                return Err("every entry of \"paths\" is a file path".to_string());
            };
            out.push(PathBuf::from(path));
        }
        if out.is_empty() {
            return Err("\"paths\" names no files".to_string());
        }
        return Ok(out);
    }
    Err("the loop tool takes \"path\" (one file) or \"paths\" (photos)".to_string())
}

/// A free name beside `input`: `<stem>-loop.<ext>`, counted up until
/// nothing stands in the way.
fn fresh_output(input: &Path, ext: &str) -> Result<PathBuf, String> {
    let dir = input.parent().unwrap_or(Path::new("."));
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("media");
    for n in 1..1000u32 {
        let name = if n == 1 {
            format!("{stem}-loop.{ext}")
        } else {
            format!("{stem}-loop-{n}.{ext}")
        };
        let candidate = dir.join(name);
        if candidate.symlink_metadata().is_err() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "no free name beside {} — a thousand -loop files already stand there",
        input.display()
    ))
}

/// A video, cross-faded into itself.
///
/// The output plays from `fade` seconds in, and its ending is a blend
/// into the opening `fade` seconds — so the frame after the last is the
/// first, and a player on repeat shows no seam. The cost is honest and
/// stated: the result is `fade` seconds shorter than the source, and v0
/// drops the audio track rather than pretending an audio splice is
/// seamless.
fn video_loop(ffmpeg: &Ffmpeg, input: &Path, course: &mut dyn Course) -> Result<Outcome, String> {
    course.progress("reading the video's length");
    let duration = probe_duration(ffmpeg, input)?;
    if duration < 0.4 {
        return Err(format!(
            "{} is {duration:.2}s long — too short to cross-fade into a loop",
            input.display()
        ));
    }
    let fade = (duration / 4.0).min(FADE_CAP_SECONDS);
    let offset = duration - 2.0 * fade;
    if course.stopped() {
        return Ok(Outcome::Cancelled);
    }

    let ext = input
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("mp4");
    // GIF back to GIF would need a palette pass; the loop goes to mp4.
    let ext = if ext.eq_ignore_ascii_case("gif") { "mp4" } else { ext };
    let output = fresh_output(input, ext)?;

    let filter = format!(
        "[0:v]split=2[a][b];\
         [a]trim=start={fade:.3},setpts=PTS-STARTPTS[main];\
         [b]trim=duration={fade:.3},setpts=PTS-STARTPTS[head];\
         [main][head]xfade=transition=fade:duration={fade:.3}:offset={offset:.3}"
    );
    course.progress("rendering the seamless loop");
    let out = ffmpeg.run(&strings(&[
        "-nostdin",
        "-hide_banner",
        "-n",
        "-i",
        &input.display().to_string(),
        "-filter_complex",
        &filter,
        "-an",
        &output.display().to_string(),
    ]))?;
    finished(out, &output)
}

/// Photos, one minute, cycling.
fn photo_loop(
    ffmpeg: &Ffmpeg,
    inputs: &[PathBuf],
    course: &mut dyn Course,
) -> Result<Outcome, String> {
    let output = fresh_output(&inputs[0], "mp4")?;
    // Even dimensions for yuv420p, whatever the photos are.
    let frame = "fps=30,scale=trunc(iw/2)*2:trunc(ih/2)*2,format=yuv420p";
    course.progress(if inputs.len() == 1 {
        "rendering one minute of the photo"
    } else {
        "rendering one minute cycling through the photos"
    });
    if course.stopped() {
        return Ok(Outcome::Cancelled);
    }

    if inputs.len() == 1 {
        let out = ffmpeg.run(&strings(&[
            "-nostdin",
            "-hide_banner",
            "-n",
            "-loop",
            "1",
            "-i",
            &inputs[0].display().to_string(),
            "-t",
            &format!("{PHOTO_CLIP_SECONDS}"),
            "-vf",
            frame,
            &output.display().to_string(),
        ]))?;
        return finished(out, &output);
    }

    // Several photos ride in through the concat demuxer, which reads a
    // list file: each photo with its share of the minute, and the first
    // named again at the end so the demuxer honours the last duration.
    let each = PHOTO_CLIP_SECONDS / inputs.len() as f64;
    let mut list = String::from("ffconcat version 1.0\n");
    for input in inputs {
        let full = std::fs::canonicalize(input)
            .map_err(|e| format!("cannot resolve {}: {e}", input.display()))?;
        let _ = writeln!(list, "file '{}'", quoted(&full));
        let _ = writeln!(list, "duration {each:.4}");
    }
    let first = std::fs::canonicalize(&inputs[0])
        .map_err(|e| format!("cannot resolve {}: {e}", inputs[0].display()))?;
    let _ = writeln!(list, "file '{}'", quoted(&first));

    let list_path = std::env::temp_dir().join(format!(
        "nacelle-ai-loop-{}-{}.ffconcat",
        std::process::id(),
        output
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("clip")
    ));
    std::fs::write(&list_path, &list)
        .map_err(|e| format!("cannot write the concat list {}: {e}", list_path.display()))?;

    let out = ffmpeg.run(&strings(&[
        "-nostdin",
        "-hide_banner",
        "-n",
        "-f",
        "concat",
        "-safe",
        "0",
        "-i",
        &list_path.display().to_string(),
        "-t",
        &format!("{PHOTO_CLIP_SECONDS}"),
        "-vf",
        frame,
        &output.display().to_string(),
    ]));
    let _ = std::fs::remove_file(&list_path);
    finished(out?, &output)
}

/// A path inside the concat list's single quotes: `'` becomes `'\''`,
/// the one escape that format defines.
fn quoted(path: &Path) -> String {
    path.display().to_string().replace('\'', "'\\''")
}

/// The clip's length, off ffmpeg's own banner: `ffmpeg -i` with no
/// output prints `Duration: HH:MM:SS.cc` on stderr and exits nonzero,
/// which is exactly the probe wanted here — reading a program's output
/// is running it, not copying it.
fn probe_duration(ffmpeg: &Ffmpeg, input: &Path) -> Result<f64, String> {
    let out = ffmpeg.run(&strings(&[
        "-nostdin",
        "-hide_banner",
        "-i",
        &input.display().to_string(),
    ]))?;
    let banner = String::from_utf8_lossy(&out.stderr);
    for line in banner.lines() {
        let Some(rest) = line.trim_start().strip_prefix("Duration:") else {
            continue;
        };
        let stamp = rest.trim_start().split([',', ' ']).next().unwrap_or("");
        if let Some(seconds) = seconds_of(stamp) {
            return Ok(seconds);
        }
    }
    Err(format!(
        "ffmpeg did not report a duration for {} — is it a video?",
        input.display()
    ))
}

/// `HH:MM:SS.cc` as seconds.
fn seconds_of(stamp: &str) -> Option<f64> {
    let mut parts = stamp.split(':');
    let hours: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next()?.parse().ok()?;
    let seconds: f64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

/// The run's verdict: ffmpeg happy and the file really there, or the
/// tail of its stderr, which is where it explains itself.
fn finished(out: Output, output: &Path) -> Result<Outcome, String> {
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = said.lines().rev().take(4).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        return Err(format!("ffmpeg failed: {}", tail.join(" / ")));
    }
    if !output.is_file() {
        return Err(format!(
            "ffmpeg reported success but {} is not there",
            output.display()
        ));
    }
    Ok(Outcome::Done(output.to_path_buf()))
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}
