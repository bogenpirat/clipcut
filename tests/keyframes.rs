//! Verifies keyframe probing against real ffprobe output.
//!
//! The unit tests cover parsing; this checks that the ffprobe invocation is
//! actually correct — right flags, right interval syntax, right stream.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use clipcut::media::{Tools, snap_target};

fn tools() -> Option<Tools> {
    Command::new("ffprobe")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    Tools::discover(None).ok()
}

/// 30 seconds at 25 fps with a keyframe every 2 seconds (`-g 50`).
fn fixture() -> PathBuf {
    static FIXTURE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    FIXTURE
        .get_or_init(|| {
            let dir = std::env::temp_dir().join("clipcut-keyframe-tests");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("gop2s.mkv");
            let tmp = dir.join(format!("gop2s.{}.partial.mkv", std::process::id()));
            let status = Command::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc=d=30:s=320x240:r=25",
                    "-c:v",
                    "libx264",
                    "-preset",
                    "ultrafast",
                    // Fixed GOP, no scene-cut keyframes, so positions are predictable.
                    "-g",
                    "50",
                    "-sc_threshold",
                    "0",
                    "-pix_fmt",
                    "yuv420p",
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

#[test]
fn probes_real_keyframes_at_the_expected_interval() {
    let Some(tools) = tools() else {
        eprintln!("skipping: ffprobe not available");
        return;
    };

    let keys = tools
        .keyframes_near(&fixture(), 20.0)
        .expect("probe should succeed");

    assert!(!keys.is_empty(), "no keyframes found in the window");
    // -g 50 at 25 fps means one every 2 s; allow for encoder discretion.
    for k in &keys {
        let nearest_even = (k / 2.0).round() * 2.0;
        assert!(
            (k - nearest_even).abs() < 0.2,
            "keyframe at {k}s is not near a 2 s boundary: {keys:?}"
        );
    }
}

#[test]
fn snap_lands_before_a_mark_placed_mid_gop() {
    let Some(tools) = tools() else {
        eprintln!("skipping: ffprobe not available");
        return;
    };

    // 15.4 s sits between keyframes at 14 s and 16 s.
    let mark = 15.4;
    let keys = tools.keyframes_near(&fixture(), mark).expect("probe");
    let snapped = snap_target(&keys, mark).expect("a preceding keyframe must exist");

    assert!(
        snapped <= mark,
        "snap must never move forward past the mark"
    );
    assert!(
        mark - snapped < 2.5,
        "snap of {:.2}s is further than one GOP",
        mark - snapped
    );
}

/// The whole point of the feature: prove the reported position is where ffmpeg
/// really starts, by cutting there and measuring the result.
#[test]
fn reported_snap_matches_what_a_stream_copy_actually_produces() {
    let Some(tools) = tools() else {
        eprintln!("skipping: ffprobe not available");
        return;
    };

    let mark = 15.4;
    let keys = tools.keyframes_near(&fixture(), mark).expect("probe");
    let snapped = snap_target(&keys, mark).expect("preceding keyframe");

    // Copy from the mark to the end of the file.
    let out = std::env::temp_dir()
        .join("clipcut-keyframe-tests")
        .join("copied.mkv");
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-ss"])
        .arg(format!("{mark}"))
        .arg("-i")
        .arg(fixture())
        .args([
            "-map",
            "0:v:0",
            "-c",
            "copy",
            "-avoid_negative_ts",
            "make_zero",
        ])
        .arg(&out)
        .status()
        .expect("spawn ffmpeg");
    assert!(status.success());

    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(&out)
        .output()
        .expect("spawn ffprobe");
    let copied: f64 = String::from_utf8_lossy(&probe.stdout)
        .trim()
        .parse()
        .expect("duration");

    // If the copy really began at `snapped`, the clip runs from there to 30 s.
    let expected = 30.0 - snapped;
    assert!(
        (copied - expected).abs() < 0.35,
        "reported snap {snapped:.2}s implies a {expected:.2}s clip, but ffmpeg produced {copied:.2}s"
    );
}
