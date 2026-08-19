//! Locating ffmpeg and asking ffprobe about a file.
//!
//! The load-bearing question here is *where a stream copy will actually start*.
//! `ffmpeg -ss ... -c copy` cannot begin mid-GOP, so it snaps back to the nearest
//! preceding keyframe — often seconds earlier than the mark. Showing that plainly
//! is the difference between a predictable tool and one that quietly hands you a
//! clip with several unwanted seconds on the front.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;

use anyhow::{Context, Result, anyhow};

/// How far back to look for a keyframe. Recordings rarely exceed a 10 s GOP;
/// 60 s is generous without making the probe expensive.
const LOOKBACK_SECONDS: f64 = 60.0;
/// A little lookahead, so a mark sitting exactly on a keyframe is recognised.
const LOOKAHEAD_SECONDS: f64 = 2.0;

/// The ffmpeg/ffprobe pair to use.
#[derive(Debug, Clone)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl Tools {
    /// Resolve the tools, preferring an explicit path over `PATH`.
    ///
    /// `override_path` may name either the ffmpeg binary itself or the directory
    /// containing it, since both are things a user might reasonably enter.
    pub fn discover(override_path: Option<&Path>) -> Result<Self> {
        if let Some(p) = override_path {
            let dir = if p.is_dir() {
                p.to_path_buf()
            } else {
                p.parent()
                    .map(Path::to_path_buf)
                    .ok_or_else(|| anyhow!("{} has no parent directory", p.display()))?
            };
            let ffmpeg = dir.join(exe("ffmpeg"));
            let ffprobe = dir.join(exe("ffprobe"));
            if ffmpeg.is_file() && ffprobe.is_file() {
                return Ok(Self { ffmpeg, ffprobe });
            }
            return Err(anyhow!(
                "no ffmpeg/ffprobe pair in {} — check the configured path",
                dir.display()
            ));
        }

        Ok(Self {
            ffmpeg: which::which("ffmpeg").context("ffmpeg is not on PATH")?,
            ffprobe: which::which("ffprobe").context("ffprobe is not on PATH")?,
        })
    }

    /// Encoder names this build supports, for greying out unavailable choices
    /// instead of failing at export time.
    pub fn encoders(&self) -> HashSet<String> {
        let Ok(out) = Command::new(&self.ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-encoders"])
            .output()
        else {
            return HashSet::new();
        };
        parse_encoders(&String::from_utf8_lossy(&out.stdout))
    }

    /// Keyframe timestamps in a window around `around`, in seconds.
    pub fn keyframes_near(&self, file: &Path, around: f64) -> Result<Vec<f64>> {
        let start = (around - LOOKBACK_SECONDS).max(0.0);
        let end = around + LOOKAHEAD_SECONDS;

        let out = Command::new(&self.ffprobe)
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_packets",
                "-show_entries",
                "packet=pts_time,flags",
                "-read_intervals",
                &format!("{start:.3}%{end:.3}"),
                "-of",
                "csv=p=0",
            ])
            .arg(file)
            .output()
            .context("could not run ffprobe")?;

        if !out.status.success() {
            return Err(anyhow!(
                "ffprobe failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(parse_keyframes(&String::from_utf8_lossy(&out.stdout)))
    }
}

#[cfg(windows)]
fn exe(name: &str) -> String {
    format!("{name}.exe")
}
#[cfg(not(windows))]
fn exe(name: &str) -> String {
    name.to_string()
}

/// Extract encoder names from `ffmpeg -encoders` output.
///
/// Lines look like ` V....D h264_nvenc           NVIDIA NVENC H.264 encoder`.
pub fn parse_encoders(text: &str) -> HashSet<String> {
    text.lines()
        .skip_while(|l| !l.starts_with(" -----"))
        .skip(1)
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(str::to_string)
        .collect()
}

/// Extract keyframe timestamps from ffprobe's `pts_time,flags` CSV.
///
/// A packet is a keyframe when its flags begin with `K`. Rows with an
/// unavailable timestamp are skipped rather than guessed at.
pub fn parse_keyframes(csv: &str) -> Vec<f64> {
    let mut times: Vec<f64> = csv
        .lines()
        .filter_map(|line| {
            let mut fields = line.trim().split(',');
            let time = fields.next()?.trim();
            let flags = fields.next()?.trim();
            if !flags.starts_with('K') {
                return None;
            }
            time.parse::<f64>().ok()
        })
        .filter(|t| t.is_finite() && *t >= 0.0)
        .collect();
    // total_cmp rather than partial_cmp().unwrap(): the NaN filter above makes the
    // unwrap safe today, but only by accident of ordering, and a panic here would
    // take down the app over malformed ffprobe output.
    times.sort_by(f64::total_cmp);
    times.dedup();
    times
}

/// The keyframe a stream copy would actually start from.
///
/// Returns the latest keyframe at or before `mark`. `None` means the window did
/// not contain one, in which case the UI should say nothing rather than show a
/// position it cannot justify.
pub fn snap_target(keyframes: &[f64], mark: f64) -> Option<f64> {
    keyframes.iter().copied().rfind(|k| *k <= mark + 1e-6)
}

// ---------------------------------------------------------------------------
// Background probing
// ---------------------------------------------------------------------------

/// Result of a keyframe probe: the mark asked about, and where a copy would start.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapResult {
    pub file: PathBuf,
    pub mark: f64,
    pub snapped: Option<f64>,
}

/// Runs keyframe probes off the UI thread, coalescing rapid requests.
///
/// Dragging a mark can produce a request per frame; only the most recent one
/// matters, so queued requests are collapsed.
pub struct SnapProbe {
    tx: mpsc::Sender<(PathBuf, f64)>,
}

impl SnapProbe {
    pub fn new<F>(tools: Tools, sink: F) -> Self
    where
        F: Fn(SnapResult) + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<(PathBuf, f64)>();
        std::thread::Builder::new()
            .name("keyframe-probe".into())
            .spawn(move || {
                while let Ok(mut request) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        request = newer;
                    }
                    let (file, mark) = request;
                    let snapped = tools
                        .keyframes_near(&file, mark)
                        .ok()
                        .and_then(|keys| snap_target(&keys, mark));
                    sink(SnapResult {
                        file,
                        mark,
                        snapped,
                    });
                }
            })
            .expect("spawn keyframe probe");
        Self { tx }
    }

    pub fn request(&self, file: PathBuf, mark: f64) {
        let _ = self.tx.send((file, mark));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyframes_are_the_rows_flagged_k() {
        let csv = "\
0.000000,K__
2.002000,__
4.004000,__
6.006000,K__
8.008000,__
";
        assert_eq!(parse_keyframes(csv), vec![0.0, 6.006]);
    }

    #[test]
    fn unavailable_timestamps_are_skipped_not_guessed() {
        let csv = "N/A,K__\n5.5,K__\n,K__\n";
        assert_eq!(parse_keyframes(csv), vec![5.5]);
    }

    #[test]
    fn malformed_rows_do_not_break_the_probe() {
        let csv = "garbage\n\n1.5,K__\nonlyonefield\n";
        assert_eq!(parse_keyframes(csv), vec![1.5]);
    }

    #[test]
    fn results_are_sorted_and_deduplicated() {
        let csv = "6.0,K__\n2.0,K__\n6.0,K__\n";
        assert_eq!(parse_keyframes(csv), vec![2.0, 6.0]);
    }

    #[test]
    fn snap_picks_the_preceding_keyframe() {
        let keys = [0.0, 4.0, 8.0, 12.0];
        assert_eq!(snap_target(&keys, 9.5), Some(8.0));
        assert_eq!(snap_target(&keys, 12.0), Some(12.0), "exact hit stays put");
        assert_eq!(snap_target(&keys, 0.0), Some(0.0));
    }

    #[test]
    fn snap_never_jumps_forward() {
        // Landing after the mark would silently drop wanted footage.
        let keys = [10.0, 20.0];
        assert_eq!(snap_target(&keys, 5.0), None);
    }

    #[test]
    fn snap_on_an_empty_window_is_unknown() {
        assert_eq!(snap_target(&[], 42.0), None);
    }

    #[test]
    fn encoder_list_is_parsed_from_the_table() {
        let text = "\
Encoders:
 V..... = Video
 ------
 V....D libx264              libx264 H.264 / AVC
 V....D h264_nvenc           NVIDIA NVENC H.264 encoder
 A....D aac                  AAC (Advanced Audio Coding)
";
        let found = parse_encoders(text);
        assert!(found.contains("libx264"));
        assert!(found.contains("h264_nvenc"));
        assert!(found.contains("aac"));
        assert!(
            !found.contains("Encoders:"),
            "the header must not be mistaken for an encoder"
        );
    }
}
