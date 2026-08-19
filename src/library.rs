//! Discovering source videos in the input folder.
//!
//! Scans recursively, sorts oldest-first (recordings accumulate chronologically,
//! so the newest work sits at the bottom where you last left off), and watches
//! the tree so files appearing while the app runs show up without a refresh.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use notify::RecursiveMode;
use notify_debouncer_full::{Debouncer, RecommendedCache, new_debouncer};
use walkdir::WalkDir;

/// Extensions treated as source video.
///
/// mpv will happily open more than this, but an allowlist keeps the list free of
/// the sidecar files that live alongside recordings.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "mov", "webm", "m4v", "avi", "ts", "m2ts", "mts", "mpg", "mpeg", "wmv", "flv",
    "ogv",
];

/// How long the filesystem must be quiet before a change is reported.
///
/// Long enough that a file still being written does not produce a burst of
/// events, short enough that a finished recording appears promptly.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(750);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFile {
    pub path: PathBuf,
    /// File name, without any directory part.
    pub display_name: String,
    /// Directory relative to the scan root; empty when directly inside it.
    pub sub_path: String,
    pub modified: SystemTime,
    pub size: u64,
}

pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            VIDEO_EXTENSIONS.contains(&e.as_str())
        })
        .unwrap_or(false)
}

/// Recursively collect video files under `root`, oldest first.
///
/// Unreadable directories are skipped rather than failing the whole scan: one
/// permission-denied subfolder should not empty the list.
pub fn scan(root: &Path) -> Vec<VideoFile> {
    if !root.is_dir() {
        return Vec::new();
    }

    let mut files: Vec<VideoFile> = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| is_video(entry.path()))
        .filter_map(|entry| {
            let path = entry.path().to_path_buf();
            let meta = entry.metadata().ok()?;
            let display_name = path.file_name()?.to_string_lossy().into_owned();
            let sub_path = path
                .parent()
                .and_then(|p| p.strip_prefix(root).ok())
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();

            Some(VideoFile {
                path,
                display_name,
                sub_path,
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                size: meta.len(),
            })
        })
        .collect();

    // Oldest first, with the name as a tiebreaker so the order is stable across
    // scans when several files share a timestamp.
    files.sort_by(|a, b| {
        a.modified
            .cmp(&b.modified)
            .then_with(|| a.display_name.cmp(&b.display_name))
    });
    files
}

/// Format a byte count for display, e.g. `1.4 GB`.
///
/// Decimal units, matching what Windows Explorer and recorders report, so the
/// number here agrees with the number the user sees elsewhere.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [(&str, f64); 4] = [("TB", 1e12), ("GB", 1e9), ("MB", 1e6), ("kB", 1e3)];
    let value = bytes as f64;
    for (unit, scale) in UNITS {
        if value >= scale {
            let n = value / scale;
            // Keep three significant figures: 9.87 GB, but 987 MB.
            return if n >= 100.0 {
                format!("{n:.0} {unit}")
            } else if n >= 10.0 {
                format!("{n:.1} {unit}")
            } else {
                format!("{n:.2} {unit}")
            };
        }
    }
    format!("{bytes} B")
}

/// Format a modification time for display, relative to today.
///
/// Recordings are usually recent, so the date is dropped when it is today and
/// the year when it is this year.
pub fn format_modified(time: SystemTime) -> String {
    use chrono::{DateTime, Datelike, Local};

    let dt: DateTime<Local> = time.into();
    let now = Local::now();

    if dt.date_naive() == now.date_naive() {
        dt.format("Today %H:%M").to_string()
    } else if dt.year() == now.year() {
        dt.format("%d %b %H:%M").to_string()
    } else {
        dt.format("%d %b %Y").to_string()
    }
}

/// Watches a folder tree, calling `on_change` after the filesystem settles.
///
/// The returned handle must be kept alive; dropping it stops the watch.
pub struct LibraryWatcher {
    _debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
}

pub fn watch<F>(root: &Path, on_change: F) -> notify::Result<LibraryWatcher>
where
    F: Fn() + Send + 'static,
{
    let mut debouncer = new_debouncer(WATCH_DEBOUNCE, None, move |result| {
        // Any event at all triggers a rescan: diffing individual events is more
        // fragile than simply re-reading a directory listing.
        if let Ok(events) = result {
            let events: Vec<notify_debouncer_full::DebouncedEvent> = events;
            if !events.is_empty() {
                on_change();
            }
        }
    })?;

    debouncer.watch(root, RecursiveMode::Recursive)?;
    Ok(LibraryWatcher {
        _debouncer: debouncer,
    })
}

/// Run a scan on a background thread and deliver the result to `sink`.
///
/// Large trees on slow disks must not block the UI.
pub fn scan_async<F>(root: PathBuf, sink: F)
where
    F: FnOnce(Vec<VideoFile>) + Send + 'static,
{
    std::thread::Builder::new()
        .name("library-scan".into())
        .spawn(move || sink(scan(&root)))
        .expect("spawn library scan");
}

/// A channel-based scan trigger that coalesces rapid requests.
///
/// The watcher can fire while a scan is already running; this makes sure only
/// the most recent request produces output.
pub struct ScanQueue {
    tx: mpsc::Sender<PathBuf>,
}

impl ScanQueue {
    pub fn new<F>(sink: F) -> Self
    where
        F: Fn(PathBuf, Vec<VideoFile>) + Send + 'static,
    {
        let (tx, rx) = mpsc::channel::<PathBuf>();
        std::thread::Builder::new()
            .name("library-scan".into())
            .spawn(move || {
                while let Ok(mut root) = rx.recv() {
                    // Collapse any requests that queued up behind this one.
                    while let Ok(newer) = rx.try_recv() {
                        root = newer;
                    }
                    let files = scan(&root);
                    sink(root, files);
                }
            })
            .expect("spawn library scan");
        Self { tx }
    }

    pub fn request(&self, root: PathBuf) {
        let _ = self.tx.send(root);
    }

    /// A `Send` trigger, for handing to the filesystem watcher.
    pub fn handle(&self) -> ScanHandle {
        ScanHandle {
            tx: self.tx.clone(),
        }
    }
}

#[derive(Clone)]
pub struct ScanHandle {
    tx: mpsc::Sender<PathBuf>,
}

impl ScanHandle {
    pub fn request(&self, root: PathBuf) {
        let _ = self.tx.send(root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, UNIX_EPOCH};

    /// A unique scratch tree per test, so parallel tests cannot collide.
    fn tree(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("clipcut-library-tests")
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Create a file and stamp its mtime, so ordering is deterministic.
    fn touch(dir: &Path, rel: &str, secs_since_epoch: u64) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, b"x").unwrap();
        let when = UNIX_EPOCH + Duration::from_secs(secs_since_epoch);
        filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(when)).unwrap();
    }

    fn names(files: &[VideoFile]) -> Vec<String> {
        files.iter().map(|f| f.display_name.clone()).collect()
    }

    #[test]
    fn recognises_video_extensions_case_insensitively() {
        assert!(is_video(Path::new("a.mkv")));
        assert!(is_video(Path::new("a.MP4")));
        assert!(is_video(Path::new("a.Mkv")));
        assert!(!is_video(Path::new("a.txt")));
        assert!(!is_video(Path::new("a.srt")), "sidecars must not be listed");
        assert!(!is_video(Path::new("noextension")));
    }

    #[test]
    fn sorts_oldest_first() {
        let dir = tree("sorting");
        touch(&dir, "newest.mkv", 3_000);
        touch(&dir, "oldest.mkv", 1_000);
        touch(&dir, "middle.mp4", 2_000);

        assert_eq!(
            names(&scan(&dir)),
            vec!["oldest.mkv", "middle.mp4", "newest.mkv"]
        );
    }

    #[test]
    fn equal_timestamps_break_ties_by_name_for_a_stable_order() {
        let dir = tree("ties");
        touch(&dir, "b.mkv", 1_000);
        touch(&dir, "a.mkv", 1_000);
        touch(&dir, "c.mkv", 1_000);
        assert_eq!(names(&scan(&dir)), vec!["a.mkv", "b.mkv", "c.mkv"]);
    }

    #[test]
    fn recurses_and_records_the_relative_folder() {
        let dir = tree("recursive");
        touch(&dir, "root.mkv", 1_000);
        touch(&dir, "session one/nested.mkv", 2_000);
        touch(&dir, "session one/deeper/deep.mkv", 3_000);

        let files = scan(&dir);
        assert_eq!(names(&files), vec!["root.mkv", "nested.mkv", "deep.mkv"]);
        assert_eq!(files[0].sub_path, "", "root files have no sub-path");
        assert_eq!(files[1].sub_path, "session one");
        assert_eq!(
            files[2].sub_path, "session one/deeper",
            "sub-paths use forward slashes regardless of platform"
        );
    }

    #[test]
    fn ignores_non_video_files() {
        let dir = tree("filtering");
        touch(&dir, "keep.mkv", 1_000);
        touch(&dir, "notes.txt", 1_000);
        touch(&dir, "subs.srt", 1_000);
        touch(&dir, "thumb.jpg", 1_000);
        assert_eq!(names(&scan(&dir)), vec!["keep.mkv"]);
    }

    #[test]
    fn missing_folder_yields_an_empty_list_rather_than_panicking() {
        let missing = std::env::temp_dir().join("clipcut-does-not-exist-9f3a");
        assert!(scan(&missing).is_empty());
    }

    #[test]
    fn a_file_path_is_not_a_valid_root() {
        let dir = tree("notadir");
        touch(&dir, "a.mkv", 1_000);
        assert!(scan(&dir.join("a.mkv")).is_empty());
    }

    #[test]
    fn sizes_are_formatted_with_three_significant_figures() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1_500), "1.50 kB");
        assert_eq!(format_size(15_000_000), "15.0 MB");
        assert_eq!(format_size(987_000_000), "987 MB");
        assert_eq!(format_size(9_870_000_000), "9.87 GB");
        assert_eq!(format_size(2_500_000_000_000), "2.50 TB");
    }

    #[test]
    fn total_size_sums_the_scan() {
        let dir = tree("totals");
        touch(&dir, "a.mkv", 1_000);
        touch(&dir, "b.mkv", 2_000);
        let total: u64 = scan(&dir).iter().map(|f| f.size).sum();
        assert_eq!(total, 2, "one byte written per fixture file");
    }

    #[test]
    fn records_size_and_modification_time() {
        let dir = tree("metadata");
        touch(&dir, "a.mkv", 12_345);
        let files = scan(&dir);
        assert_eq!(files[0].size, 1);
        assert_eq!(files[0].modified, UNIX_EPOCH + Duration::from_secs(12_345));
    }
}
