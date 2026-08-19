//! Persisted settings.
//!
//! Every setting is saved the moment it changes, which naively would mean a disk
//! write per pixel of slider travel. Instead writes are **debounced** (quiet
//! period) with a **maximum delay** so a long drag still checkpoints, and are
//! **atomic** (write to a temp file, then rename) so a crash mid-write cannot
//! leave a truncated config behind.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::encode::EncodeSettings;

const DEBOUNCE: Duration = Duration::from_millis(400);
/// Longest a change may sit unwritten while updates keep arriving.
const MAX_DELAY: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowState {
    pub width: u32,
    pub height: u32,
    /// Top-left in physical pixels. `None` lets the window manager place it,
    /// which is the right behaviour on a first run.
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub maximized: bool,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 800,
            x: None,
            y: None,
            maximized: false,
        }
    }
}

impl WindowState {
    /// Whether a saved position is worth restoring.
    ///
    /// A monitor that has been unplugged since the last run would otherwise put
    /// the window somewhere unreachable, so absurd coordinates are discarded.
    pub fn usable_position(&self) -> Option<(i32, i32)> {
        let (x, y) = (self.x?, self.y?);
        (x > -32_000 && x < 32_000 && y > -32_000 && y < 32_000).then_some((x, y))
    }
}

/// Unknown keys are ignored and missing keys fall back to defaults, so a config
/// written by a newer or older build still loads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub input_folder: Option<PathBuf>,
    pub output_folder: Option<PathBuf>,
    /// mpv's scale: 0-130, where 100 is unattenuated.
    pub volume: f64,
    pub muted: bool,
    pub encoding: EncodeSettings,
    pub encoding_panel_expanded: bool,
    pub output_panel_expanded: bool,
    /// Explicit ffmpeg location; `None` means "find it on PATH".
    pub ffmpeg_path: Option<PathBuf>,
    pub window: WindowState,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            input_folder: None,
            output_folder: None,
            volume: 100.0,
            muted: false,
            encoding: EncodeSettings::default(),
            encoding_panel_expanded: false,
            output_panel_expanded: true,
            ffmpeg_path: None,
            window: WindowState::default(),
        }
    }
}

impl Settings {
    /// Clamp anything a hand-edited config could put out of range.
    fn sanitized(mut self) -> Self {
        if !self.volume.is_finite() {
            self.volume = 100.0;
        }
        self.volume = self.volume.clamp(0.0, 130.0);
        self.window.width = self.window.width.clamp(640, 16_384);
        self.window.height = self.window.height.clamp(480, 16_384);
        self
    }
}

/// `%APPDATA%\clipcut\config.toml`, or the platform equivalent.
pub fn default_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "clipcut").map(|d| d.config_dir().join("config.toml"))
}

pub fn load_from(path: &Path) -> Settings {
    let Ok(text) = fs::read_to_string(path) else {
        return Settings::default();
    };
    match toml::from_str::<Settings>(&text) {
        Ok(s) => s.sanitized(),
        Err(e) => {
            eprintln!(
                "warning: {} is not valid, using defaults: {e}",
                path.display()
            );
            Settings::default()
        }
    }
}

/// Serialize to a temp file and rename over the target.
///
/// The rename is what makes this atomic: readers see either the old file or the
/// new one, never a half-written one.
pub fn write_atomic(path: &Path, settings: &Settings) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let text = toml::to_string_pretty(settings)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

/// Owns the background writer. Dropping it flushes any pending write.
pub struct ConfigStore {
    path: PathBuf,
    tx: Option<Sender<Settings>>,
    worker: Option<JoinHandle<()>>,
}

impl ConfigStore {
    pub fn open_at(path: PathBuf) -> (Settings, Self) {
        Self::open_with(path, DEBOUNCE, MAX_DELAY)
    }

    /// As [`Self::open_at`], with explicit timings. Exists for tests.
    pub fn open_with(path: PathBuf, debounce: Duration, max_delay: Duration) -> (Settings, Self) {
        let settings = load_from(&path);
        let (tx, rx) = mpsc::channel();
        let worker_path = path.clone();
        let worker = std::thread::Builder::new()
            .name("config-writer".into())
            .spawn(move || run_writer(worker_path, rx, debounce, max_delay))
            .expect("spawn config writer");

        (
            settings,
            Self {
                path,
                tx: Some(tx),
                worker: Some(worker),
            },
        )
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Queue a save. Cheap enough to call on every keystroke or slider tick.
    pub fn store(&self, settings: &Settings) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(settings.clone());
        }
    }

    /// Write immediately, bypassing the debounce.
    ///
    /// Call this on shutdown. Do not rely on `Drop` for the final write: the
    /// store is usually held behind an `Rc` shared with UI callbacks, so the
    /// last handle may outlive the point where the flush needed to happen.
    pub fn save_now(&self, settings: &Settings) {
        flush(&self.path, settings);
    }
}

impl Drop for ConfigStore {
    fn drop(&mut self) {
        // Closing the channel tells the writer to flush and exit.
        drop(self.tx.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_writer(path: PathBuf, rx: Receiver<Settings>, debounce: Duration, max_delay: Duration) {
    let mut pending: Option<Settings> = None;
    let mut oldest: Option<Instant> = None;

    loop {
        // Wait indefinitely when idle; otherwise until the quiet period elapses,
        // capped so a continuous stream of changes still gets checkpointed.
        let timeout = match oldest {
            None => Duration::from_secs(3600),
            Some(first) => debounce.min(max_delay.saturating_sub(first.elapsed())),
        };

        match rx.recv_timeout(timeout) {
            Ok(settings) => {
                if pending.is_none() {
                    oldest = Some(Instant::now());
                }
                pending = Some(settings);
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(settings) = pending.take() {
                    oldest = None;
                    flush(&path, &settings);
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                if let Some(settings) = pending.take() {
                    flush(&path, &settings);
                }
                break;
            }
        }
    }
}

fn flush(path: &Path, settings: &Settings) {
    if let Err(e) = write_atomic(path, settings) {
        eprintln!(
            "warning: could not save settings to {}: {e}",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::{Container, CutMode, VideoEncoder};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("clipcut-config-tests");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("{name}.toml"));
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn round_trips_through_disk() {
        let path = scratch("roundtrip");
        let s = Settings {
            input_folder: Some(PathBuf::from(r"D:\recordings")),
            volume: 73.5,
            muted: true,
            encoding: EncodeSettings {
                mode: CutMode::Fast,
                video: VideoEncoder::NvencAv1,
                container: Container::Mkv,
                ..EncodeSettings::default()
            },
            ..Settings::default()
        };

        write_atomic(&path, &s).unwrap();
        assert_eq!(load_from(&path), s);
    }

    #[test]
    fn missing_file_yields_defaults() {
        let path = scratch("missing");
        assert_eq!(load_from(&path), Settings::default());
    }

    #[test]
    fn corrupt_file_yields_defaults_rather_than_failing() {
        let path = scratch("corrupt");
        fs::write(&path, "this is not toml {{{").unwrap();
        assert_eq!(load_from(&path), Settings::default());
    }

    #[test]
    fn partial_config_fills_the_gaps() {
        let path = scratch("partial");
        fs::write(&path, "volume = 42.0\n").unwrap();
        let s = load_from(&path);
        assert_eq!(s.volume, 42.0);
        // Everything else must still be usable.
        assert_eq!(s.encoding, EncodeSettings::default());
        assert_eq!(s.window, WindowState::default());
    }

    #[test]
    fn unknown_keys_are_ignored_for_forward_compatibility() {
        let path = scratch("unknown");
        fs::write(&path, "volume = 55.0\nsome_future_key = \"hello\"\n").unwrap();
        assert_eq!(load_from(&path).volume, 55.0);
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let path = scratch("range");
        fs::write(&path, "volume = 9999.0\n[window]\nwidth = 1\nheight = 1\n").unwrap();
        let s = load_from(&path);
        assert_eq!(s.volume, 130.0, "volume clamped to mpv's maximum");
        assert!(s.window.width >= 640 && s.window.height >= 480);
    }

    #[test]
    fn nan_volume_does_not_propagate() {
        let path = scratch("nan");
        fs::write(&path, "volume = nan\n").unwrap();
        assert_eq!(load_from(&path).volume, 100.0);
    }

    #[test]
    fn writes_are_coalesced_and_flushed_on_drop() {
        let path = scratch("debounce");
        let (_loaded, store) = ConfigStore::open_with(
            path.clone(),
            Duration::from_millis(50),
            Duration::from_secs(10),
        );

        // A burst, as produced by dragging a slider.
        let mut s = Settings::default();
        for v in 1..=25 {
            s.volume = v as f64;
            store.store(&s);
        }

        // Dropping flushes whatever is still pending.
        drop(store);

        let saved = load_from(&path);
        assert_eq!(saved.volume, 25.0, "the last value in the burst must win");
    }

    #[test]
    fn max_delay_checkpoints_a_continuous_stream() {
        let path = scratch("maxdelay");
        let (_loaded, store) = ConfigStore::open_with(
            path.clone(),
            Duration::from_millis(40),
            Duration::from_millis(80),
        );

        // Keep sending faster than the debounce so it can never go quiet.
        let mut s = Settings::default();
        let deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < deadline {
            s.volume = 61.0;
            store.store(&s);
            std::thread::sleep(Duration::from_millis(10));
        }

        // A pure debounce would still not have written anything by now.
        let saved = load_from(&path);
        assert_eq!(
            saved.volume, 61.0,
            "max delay must force a checkpoint during a sustained stream"
        );
    }

    #[test]
    fn window_position_round_trips() {
        let path = scratch("winpos");
        let s = Settings {
            window: WindowState {
                x: Some(120),
                y: Some(-40),
                ..WindowState::default()
            },
            ..Settings::default()
        };
        write_atomic(&path, &s).unwrap();
        assert_eq!(load_from(&path).window.usable_position(), Some((120, -40)));
    }

    #[test]
    fn an_absent_position_is_not_restored() {
        // First run: let the window manager decide.
        assert_eq!(WindowState::default().usable_position(), None);
    }

    #[test]
    fn an_offscreen_position_is_discarded() {
        // A monitor unplugged since the last run would strand the window.
        let far = WindowState {
            x: Some(999_999),
            y: Some(10),
            ..WindowState::default()
        };
        assert_eq!(far.usable_position(), None);
    }

    #[test]
    fn no_temp_file_is_left_behind() {
        let path = scratch("tmp");
        write_atomic(&path, &Settings::default()).unwrap();
        assert!(path.exists());
        assert!(
            !path.with_extension("toml.tmp").exists(),
            "the temp file must be renamed away, not left on disk"
        );
    }
}
