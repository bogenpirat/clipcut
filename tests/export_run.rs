//! Integration tests for running exports: success, cancellation and failure.
//!
//! These drive real ffmpeg processes, which is the only way to check that
//! cancellation actually kills the child and that failures carry ffmpeg's own
//! diagnostics rather than a generic message.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use clipcut::encode::{
    AudioHandling, Container, CutMode, EncodeSettings, ExportRequest, Speed, VideoEncoder,
};
use clipcut::export::{ExportEvent, start, unique_output_path};
use clipcut::media::Tools;

fn tools() -> Option<Tools> {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    Tools::discover(None).ok()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("clipcut-export-run").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 60 seconds of 720p, long enough that an export can be cancelled mid-flight.
fn fixture() -> PathBuf {
    static FIXTURE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    FIXTURE
        .get_or_init(|| {
            let dir = std::env::temp_dir().join("clipcut-export-run");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("source.mkv");
            let tmp = dir.join(format!("source.{}.partial.mkv", std::process::id()));
            let status = Command::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc=d=60:s=1280x720:r=30",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=d=60",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    "-g",
                    "60",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-shortest",
                ])
                .arg(&tmp)
                .status()
                .expect("spawn ffmpeg");
            assert!(status.success());
            std::fs::rename(&tmp, &path).unwrap();
            path
        })
        .clone()
}

fn request(output: PathBuf, duration: f64, speed: Speed) -> ExportRequest {
    ExportRequest {
        input: fixture(),
        output,
        start: 5.0,
        duration,
        audio_track: None,
        settings: EncodeSettings {
            mode: CutMode::Precise,
            video: VideoEncoder::X264,
            quality: 28,
            speed,
            audio: AudioHandling::Aac { kbps: 128 },
            container: Container::Mp4,
        },
    }
}

/// Collect events until a terminal one arrives.
fn run_to_completion(
    tools: &Tools,
    req: ExportRequest,
    cancel_after: Option<Duration>,
) -> Vec<ExportEvent> {
    let (tx, rx) = mpsc::channel();
    let handle = start(tools, req, move |e| {
        let _ = tx.send(e);
    })
    .expect("export should start");

    if let Some(delay) = cancel_after {
        std::thread::sleep(delay);
        handle.cancel();
    }

    let mut events = Vec::new();
    while let Ok(event) = rx.recv_timeout(Duration::from_secs(60)) {
        let terminal = !matches!(event, ExportEvent::Progress(_));
        events.push(event);
        if terminal {
            break;
        }
    }
    events
}

#[test]
fn a_successful_export_reports_progress_then_finishes() {
    let Some(tools) = tools() else {
        eprintln!("skipping: ffmpeg not available");
        return;
    };
    let out = scratch("success").join("clip.mp4");
    let events = run_to_completion(&tools, request(out.clone(), 3.0, Speed::Fastest), None);

    assert!(
        events.iter().any(|e| matches!(e, ExportEvent::Progress(_))),
        "no progress was reported: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(ExportEvent::Finished { .. })),
        "expected Finished, got {:?}",
        events.last()
    );
    assert!(out.exists(), "the clip should exist on success");
}

#[test]
fn cancelling_kills_ffmpeg_and_removes_the_partial_file() {
    let Some(tools) = tools() else {
        eprintln!("skipping: ffmpeg not available");
        return;
    };
    let out = scratch("cancel").join("clip.mp4");
    // Slow encoder over a long span, so there is time to cancel mid-flight.
    let events = run_to_completion(
        &tools,
        request(out.clone(), 50.0, Speed::Slowest),
        Some(Duration::from_millis(700)),
    );

    assert!(
        matches!(events.last(), Some(ExportEvent::Cancelled)),
        "expected Cancelled, got {:?}",
        events.last()
    );
    assert!(
        !out.exists(),
        "a cancelled export must not leave a half-written clip behind"
    );
}

#[test]
fn a_failure_surfaces_ffmpegs_own_message() {
    let Some(tools) = tools() else {
        eprintln!("skipping: ffmpeg not available");
        return;
    };
    let dir = scratch("failure");
    let mut req = request(dir.join("clip.mp4"), 3.0, Speed::Fastest);
    req.input = dir.join("this-file-does-not-exist.mkv");

    let events = run_to_completion(&tools, req, None);

    match events.last() {
        Some(ExportEvent::Failed { message }) => {
            // A generic "export failed" would leave the user with nothing to act on.
            assert!(
                !message.trim().is_empty(),
                "the failure message must not be empty"
            );
            assert!(
                message.to_lowercase().contains("no such file")
                    || message.to_lowercase().contains("does-not-exist"),
                "message should name the real problem, got: {message}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// MP3 output must actually be audio-only and playable — the argv tests prove
/// the flags, only running ffmpeg proves the file.
#[test]
fn mp3_export_produces_an_audio_only_file() {
    let Some(tools) = tools() else {
        eprintln!("skipping: ffmpeg not available");
        return;
    };
    let out = scratch("mp3").join("clip.mp3");
    let mut req = request(out.clone(), 4.0, Speed::Fastest);
    req.settings.container = Container::Mp3;

    let events = run_to_completion(&tools, req, None);
    assert!(
        matches!(events.last(), Some(ExportEvent::Finished { .. })),
        "expected Finished, got {:?}",
        events.last()
    );

    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_type,codec_name",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(&out)
        .output()
        .expect("spawn ffprobe");
    let streams = String::from_utf8_lossy(&probe.stdout).to_lowercase();

    assert!(
        streams.contains("mp3"),
        "expected an mp3 stream, got: {streams}"
    );
    assert!(
        !streams.contains("video"),
        "an audio-only export must contain no video stream, got: {streams}"
    );
}

#[test]
fn exporting_twice_does_not_overwrite() {
    let dir = scratch("collision");
    let first = unique_output_path(&dir, "clip", "mp4");
    std::fs::write(&first, b"pretend this is a clip").unwrap();

    let second = unique_output_path(&dir, "clip", "mp4");
    assert_ne!(first, second);
    assert!(first.exists(), "the earlier clip must survive");
    assert!(!second.exists());
}
