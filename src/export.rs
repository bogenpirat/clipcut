//! Running ffmpeg to produce a clip.
//!
//! The job runs as a child process so a decoder crash kills the export rather
//! than the app, cancellation is a plain `kill()`, and the exact command stays
//! reproducible in a terminal.
//!
//! Failures surface ffmpeg's own diagnostics. A generic "export failed" is
//! useless; `Unknown encoder 'av1_nvenc'` tells you what to change.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::encode::{ExportRequest, Progress, ProgressParser, build_args, display_command};
use crate::media::{Tools, command};

const STDERR_TAIL_LINES: usize = 40;

#[derive(Debug, Clone)]
pub enum ExportEvent {
    Progress(Progress),
    Finished { output: PathBuf },
    Failed { message: String },
    Cancelled,
}

pub struct ExportHandle {
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
}

impl ExportHandle {
    /// Stop the export and discard the partial file.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.child.lock()
            && let Some(child) = guard.as_mut()
        {
            let _ = child.kill();
        }
    }
}

/// Start an export, reporting progress and the outcome through `sink`.
///
/// `sink` is called from a worker thread and must marshal to the UI itself.
pub fn start<F>(tools: &Tools, request: ExportRequest, sink: F) -> Result<ExportHandle>
where
    F: Fn(ExportEvent) + Send + 'static,
{
    let args = build_args(&request);
    let rendered = display_command(&tools.ffmpeg, &args);
    println!("EXPORT {rendered}");

    if let Some(parent) = request.output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }

    let mut child = command(&tools.ffmpeg)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not start ffmpeg: {}", tools.ffmpeg.display()))?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    let cancelled = Arc::new(AtomicBool::new(false));
    let child = Arc::new(Mutex::new(Some(child)));

    // ffmpeg writes diagnostics to stderr continuously; keep only the tail so a
    // chatty encoder cannot exhaust memory on a long export.
    let stderr_tail = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let tail = stderr_tail.clone();
        std::thread::Builder::new()
            .name("ffmpeg-stderr".into())
            .spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let mut tail = tail.lock().unwrap();
                    if tail.len() == STDERR_TAIL_LINES {
                        tail.remove(0);
                    }
                    tail.push(line);
                }
            })
            .expect("spawn stderr reader");
    }

    {
        let child = child.clone();
        let cancelled = cancelled.clone();
        let output = request.output.clone();
        std::thread::Builder::new()
            .name("ffmpeg-progress".into())
            .spawn(move || {
                let mut parser = ProgressParser::new();
                let mut reader = BufReader::new(stdout);
                let mut buf = [0u8; 4096];

                // Read raw rather than by line: `-progress` output is only
                // flushed in blocks, and the parser already handles split lines.
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let text = String::from_utf8_lossy(&buf[..n]);
                            for progress in parser.push_str(&text) {
                                sink(ExportEvent::Progress(progress));
                            }
                        }
                    }
                }

                let status = {
                    let mut guard = child.lock().unwrap();
                    guard.as_mut().map(|c| c.wait())
                };

                if cancelled.load(Ordering::SeqCst) {
                    // The partial file is not a usable clip.
                    let _ = std::fs::remove_file(&output);
                    sink(ExportEvent::Cancelled);
                    return;
                }

                match status {
                    Some(Ok(status)) if status.success() => sink(ExportEvent::Finished { output }),
                    Some(Ok(status)) => {
                        let tail = stderr_tail.lock().unwrap();
                        let detail = tail
                            .iter()
                            .rev()
                            .find(|l| !l.trim().is_empty())
                            .cloned()
                            .unwrap_or_else(|| format!("ffmpeg exited with {status}"));
                        let _ = std::fs::remove_file(&output);
                        sink(ExportEvent::Failed { message: detail });
                    }
                    Some(Err(e)) => sink(ExportEvent::Failed {
                        message: format!("could not wait for ffmpeg: {e}"),
                    }),
                    None => sink(ExportEvent::Failed {
                        message: "ffmpeg process disappeared".into(),
                    }),
                }
            })
            .expect("spawn progress reader");
    }

    Ok(ExportHandle { child, cancelled })
}

/// Strip characters that cannot appear in a filename.
///
/// Applied to whatever the user typed, so a stray `/` or `:` produces a sane
/// name rather than a write to an unexpected directory.
pub fn sanitize_stem(input: &str) -> String {
    let cleaned: String = input
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    // Windows rejects trailing dots and spaces.
    let trimmed = cleaned.trim().trim_end_matches('.').trim();
    if trimmed.is_empty() {
        "clip".to_string()
    } else {
        trimmed.to_string()
    }
}

/// A path in `dir` that does not exist yet, based on `stem`.
///
/// Overwriting silently is the one outcome to avoid: the source of a clip is
/// often the only copy of a moment. A trailing `_NNN` is treated as a counter
/// and advanced; otherwise one is appended.
pub fn unique_output_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let stem = sanitize_stem(stem);
    let first = dir.join(format!("{stem}.{ext}"));
    if !first.exists() {
        return first;
    }

    let (root, start) = split_counter(&stem);
    for n in (start + 1)..=9999 {
        let candidate = dir.join(format!("{root}_{n:03}.{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    // Pathological case: fall back to something unique rather than overwriting.
    dir.join(format!("{root}_{}.{ext}", std::process::id()))
}

fn split_counter(stem: &str) -> (String, u32) {
    if let Some((root, digits)) = stem.rsplit_once('_')
        && !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
        && let Ok(n) = digits.parse::<u32>()
    {
        return (root.to_string(), n);
    }
    (stem.to_string(), 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("clipcut-export-tests").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn illegal_characters_are_replaced() {
        assert_eq!(sanitize_stem("a/b\\c:d"), "a_b_c_d");
        assert_eq!(sanitize_stem("clip<1>"), "clip_1_");
        assert_eq!(sanitize_stem("what?"), "what_");
    }

    #[test]
    fn trailing_dots_and_spaces_are_trimmed() {
        // Windows silently mangles these, which would break collision checks.
        assert_eq!(sanitize_stem("  clip  "), "clip");
        assert_eq!(sanitize_stem("clip..."), "clip");
    }

    #[test]
    fn an_empty_name_falls_back() {
        assert_eq!(sanitize_stem(""), "clip");
        assert_eq!(sanitize_stem("   "), "clip");
        assert_eq!(sanitize_stem("..."), "clip");
    }

    #[test]
    fn a_free_name_is_used_as_is() {
        let dir = scratch("free");
        assert_eq!(
            unique_output_path(&dir, "clip", "mp4"),
            dir.join("clip.mp4")
        );
    }

    #[test]
    fn an_existing_file_is_never_overwritten() {
        let dir = scratch("collide");
        std::fs::write(dir.join("clip.mp4"), b"x").unwrap();
        let next = unique_output_path(&dir, "clip", "mp4");
        assert_eq!(next, dir.join("clip_001.mp4"));
        assert!(!next.exists());
    }

    #[test]
    fn a_trailing_counter_is_advanced_not_appended() {
        let dir = scratch("counter");
        std::fs::write(dir.join("session_001.mp4"), b"x").unwrap();
        assert_eq!(
            unique_output_path(&dir, "session_001", "mp4"),
            dir.join("session_002.mp4"),
            "should advance the counter rather than produce session_001_001"
        );
    }

    #[test]
    fn gaps_in_the_sequence_are_filled() {
        let dir = scratch("gaps");
        for n in ["clip_001", "clip_002", "clip_004"] {
            std::fs::write(dir.join(format!("{n}.mp4")), b"x").unwrap();
        }
        std::fs::write(dir.join("clip.mp4"), b"x").unwrap();
        assert_eq!(
            unique_output_path(&dir, "clip", "mp4"),
            dir.join("clip_003.mp4")
        );
    }

    #[test]
    fn the_extension_is_part_of_the_collision_check() {
        let dir = scratch("ext");
        std::fs::write(dir.join("clip.mp4"), b"x").unwrap();
        // A different container is not a collision.
        assert_eq!(
            unique_output_path(&dir, "clip", "mkv"),
            dir.join("clip.mkv")
        );
    }

    #[test]
    fn counter_splitting_handles_awkward_stems() {
        assert_eq!(split_counter("clip_001"), ("clip".into(), 1));
        assert_eq!(split_counter("clip"), ("clip".into(), 0));
        assert_eq!(split_counter("clip_"), ("clip_".into(), 0));
        assert_eq!(split_counter("clip_abc"), ("clip_abc".into(), 0));
        assert_eq!(split_counter("a_b_007"), ("a_b".into(), 7));
    }
}
