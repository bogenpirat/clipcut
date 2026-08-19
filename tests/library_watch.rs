//! Integration tests for the filesystem watcher and scan queue.
//!
//! These involve real timing and real filesystem events, which unit tests of
//! `scan()` cannot cover.

use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use clipcut::library::{ScanQueue, VideoFile, watch};

fn tree(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("clipcut-watch-tests").join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Wait for a result, failing with context rather than hanging forever.
fn expect_within(
    rx: &mpsc::Receiver<Vec<VideoFile>>,
    timeout: Duration,
    what: &str,
) -> Vec<VideoFile> {
    rx.recv_timeout(timeout)
        .unwrap_or_else(|_| panic!("timed out after {timeout:?} waiting for {what}"))
}

#[test]
fn scan_queue_delivers_results() {
    let dir = tree("queue");
    fs::write(dir.join("a.mkv"), b"x").unwrap();

    let (tx, rx) = mpsc::channel();
    let queue = ScanQueue::new(move |_root, files| {
        let _ = tx.send(files);
    });

    queue.request(dir.clone());
    let files = expect_within(&rx, Duration::from_secs(5), "the initial scan");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].display_name, "a.mkv");
}

#[test]
fn rapid_requests_are_coalesced() {
    let dir = tree("coalesce");
    for i in 0..3 {
        fs::write(dir.join(format!("{i}.mkv")), b"x").unwrap();
    }

    let (tx, rx) = mpsc::channel();
    let queue = ScanQueue::new(move |_root, files| {
        let _ = tx.send(files);
    });

    // A burst, as the watcher produces when several files land at once.
    for _ in 0..20 {
        queue.request(dir.clone());
    }

    // Collect everything that arrives over a short window.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut scans = 0;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(files) => {
                assert_eq!(files.len(), 3, "every scan must see all three files");
                scans += 1;
            }
            Err(_) => break,
        }
    }

    assert!(scans >= 1, "at least one scan must run");
    assert!(
        scans < 20,
        "20 requests produced {scans} scans; they are not being coalesced"
    );
}

#[test]
fn watcher_notices_a_new_file() {
    let dir = tree("watch-new");
    fs::write(dir.join("existing.mkv"), b"x").unwrap();

    let (tx, rx) = mpsc::channel();
    let queue = ScanQueue::new(move |_root, files| {
        let _ = tx.send(files);
    });

    let handle = queue.handle();
    let watched = dir.clone();
    let _watcher = watch(&dir, move || handle.request(watched.clone())).expect("start watcher");

    // Drain the initial scan so the assertion below is about the watcher.
    queue.request(dir.clone());
    let initial = expect_within(&rx, Duration::from_secs(5), "the initial scan");
    assert_eq!(initial.len(), 1);

    // Drop a file in, as a recorder would.
    fs::write(dir.join("arrived.mkv"), b"x").unwrap();

    // The debouncer waits for the filesystem to settle before reporting.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "watcher never reported the new file");
        if let Ok(files) = rx.recv_timeout(remaining)
            && files.len() == 2
        {
            let names: Vec<&str> = files.iter().map(|f| f.display_name.as_str()).collect();
            assert!(names.contains(&"arrived.mkv"));
            break;
        }
    }
}

/// A file removed outside the app — in Explorer, or by another tool — must
/// disappear from the list without needing a manual refresh.
#[test]
fn watcher_notices_a_deleted_file() {
    let dir = tree("watch-delete");
    fs::write(dir.join("keep.mkv"), b"x").unwrap();
    fs::write(dir.join("doomed.mkv"), b"x").unwrap();

    let (tx, rx) = mpsc::channel();
    let queue = ScanQueue::new(move |_root, files| {
        let _ = tx.send(files);
    });

    let handle = queue.handle();
    let watched = dir.clone();
    let _watcher = watch(&dir, move || handle.request(watched.clone())).expect("start watcher");

    queue.request(dir.clone());
    let initial = expect_within(&rx, Duration::from_secs(5), "the initial scan");
    assert_eq!(initial.len(), 2);

    fs::remove_file(dir.join("doomed.mkv")).unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "watcher never reported the deleted file"
        );
        if let Ok(files) = rx.recv_timeout(remaining)
            && files.len() == 1
        {
            assert_eq!(files[0].display_name, "keep.mkv");
            break;
        }
    }
}

#[test]
fn watcher_notices_a_file_in_a_new_subfolder() {
    let dir = tree("watch-nested");
    let (tx, rx) = mpsc::channel();
    let queue = ScanQueue::new(move |_root, files| {
        let _ = tx.send(files);
    });

    let handle = queue.handle();
    let watched = dir.clone();
    let _watcher = watch(&dir, move || handle.request(watched.clone())).expect("start watcher");

    // Recursive watching must cover folders created after the watch started.
    fs::create_dir_all(dir.join("session")).unwrap();
    fs::write(dir.join("session").join("clip.mkv"), b"x").unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "watcher never reported the nested file"
        );
        if let Ok(files) = rx.recv_timeout(remaining)
            && files.len() == 1
        {
            assert_eq!(files[0].display_name, "clip.mkv");
            assert_eq!(files[0].sub_path, "session");
            break;
        }
    }
}
