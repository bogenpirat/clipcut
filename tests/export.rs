//! End-to-end checks that the argument vectors we build are actually accepted by
//! ffmpeg and produce clips of the right length and codec.
//!
//! Unit tests cover the shape of the argv; only running ffmpeg proves the flags
//! are real. Skipped automatically when ffmpeg is not on PATH.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use clipcut::encode::{
    AudioHandling, Container, CutMode, EncodeSettings, ExportRequest, ProgressParser, Speed,
    VideoEncoder, build_args, display_command,
};

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join("clipcut-tests");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// A 6-second 320x240 source with a keyframe every second.
///
/// Built exactly once even though tests run in parallel, and published by
/// rename so a partially written file is never observable.
fn fixture() -> PathBuf {
    static FIXTURE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    FIXTURE
        .get_or_init(|| {
            let path = scratch().join("fixture.mkv");
            let tmp = scratch().join(format!("fixture.{}.partial.mkv", std::process::id()));
            let status = Command::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc=d=6:s=320x240:r=25",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=d=6",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-g",
                    "25",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-shortest",
                ])
                .arg(&tmp)
                .status()
                .expect("spawn ffmpeg");
            assert!(status.success(), "failed to build fixture");
            std::fs::rename(&tmp, &path).expect("publish fixture");
            path
        })
        .clone()
}

fn probe(path: &Path, entries: &str) -> String {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            entries,
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn duration_of(path: &Path) -> f64 {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .expect("spawn ffprobe");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(-1.0)
}

/// Run an export, returning the progress snapshots parsed from real ffmpeg output.
fn run_export(req: &ExportRequest) -> Vec<clipcut::encode::Progress> {
    let args = build_args(req);
    let rendered = display_command(Path::new("ffmpeg"), &args);

    let out = Command::new("ffmpeg")
        .args(&args)
        .output()
        .expect("spawn ffmpeg");

    assert!(
        out.status.success(),
        "ffmpeg failed\n  command: {rendered}\n  stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut parser = ProgressParser::new();
    parser.push_str(&String::from_utf8_lossy(&out.stdout))
}

#[test]
fn precise_export_is_exact_and_reencodes() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let out = scratch().join("precise.mp4");
    let req = ExportRequest {
        input: fixture(),
        output: out.clone(),
        start: 2.0,
        duration: 2.0,
        audio_track: None,
        settings: EncodeSettings {
            mode: CutMode::Precise,
            video: VideoEncoder::X264,
            quality: 28,
            speed: Speed::Fastest,
            audio: AudioHandling::Aac { kbps: 128 },
            container: Container::Mp4,
        },
    };

    let progress = run_export(&req);

    let dur = duration_of(&out);
    assert!(
        (dur - 2.0).abs() < 0.15,
        "re-encode should honour the marks exactly, got {dur}s"
    );
    assert_eq!(probe(&out, "stream=codec_name"), "h264");
    assert_eq!(probe(&out, "stream=pix_fmt"), "yuv420p");

    // The parser must cope with genuine ffmpeg output, not just our fixtures.
    assert!(
        !progress.is_empty(),
        "no progress blocks parsed from ffmpeg"
    );
    assert!(
        progress.last().unwrap().finished,
        "final block should be progress=end"
    );
    assert!(
        progress.last().unwrap().fraction(2.0) >= 0.99,
        "progress should reach completion"
    );
}

#[test]
fn nvenc_constant_quality_flags_are_accepted() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    // Only meaningful where an NVIDIA encoder exists.
    let encoders = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .expect("spawn ffmpeg");
    if !String::from_utf8_lossy(&encoders.stdout).contains("h264_nvenc") {
        eprintln!("skipping: h264_nvenc not available in this build");
        return;
    }

    let out = scratch().join("nvenc.mp4");
    let req = ExportRequest {
        input: fixture(),
        output: out.clone(),
        start: 1.0,
        duration: 2.0,
        audio_track: None,
        settings: EncodeSettings {
            mode: CutMode::Precise,
            video: VideoEncoder::NvencH264,
            quality: 30,
            speed: Speed::Fast,
            audio: AudioHandling::Aac { kbps: 128 },
            container: Container::Mp4,
        },
    };

    run_export(&req);
    assert_eq!(probe(&out, "stream=codec_name"), "h264");
    let dur = duration_of(&out);
    assert!((dur - 2.0).abs() < 0.15, "expected ~2s, got {dur}s");
}

#[test]
fn fast_export_copies_the_stream() {
    if !ffmpeg_available() {
        eprintln!("skipping: ffmpeg not on PATH");
        return;
    }
    let out = scratch().join("fast.mkv");
    let req = ExportRequest {
        input: fixture(),
        output: out.clone(),
        // Deliberately off a keyframe: the fixture has one per second.
        start: 2.4,
        duration: 2.0,
        audio_track: None,
        settings: EncodeSettings {
            mode: CutMode::Fast,
            container: Container::Mkv,
            ..Default::default()
        },
    };

    run_export(&req);
    assert_eq!(probe(&out, "stream=codec_name"), "h264");

    // Stream copy snaps the start back to the preceding keyframe, so the clip
    // runs long. This is exactly the behaviour the UI must warn about.
    let dur = duration_of(&out);
    assert!(
        dur >= 1.9,
        "copy should yield at least the requested span, got {dur}s"
    );
}
