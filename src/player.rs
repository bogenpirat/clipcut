//! mpv playback control.
//!
//! Deliberately knows nothing about the UI: state changes are reported as
//! [`PlayerEvent`]s through a callback, so the GUI decides how to marshal them.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use libmpv2::events::{Event, PropertyData};
use libmpv2::{Format, Mpv};

/// Reply IDs for observed properties.
const ID_TIME_POS: u64 = 1;
const ID_DURATION: u64 = 2;
const ID_PAUSE: u64 = 3;

#[derive(Debug, Clone, PartialEq)]
pub enum PlayerEvent {
    Position(f64),
    Duration(f64),
    Paused(bool),
    FileLoaded,
    EndOfFile,
    /// A seek finished. Carries the latency, which the UI surfaces while scrubbing.
    SeekCompleted(Duration),
    Shutdown,
}

pub struct Player {
    /// Leaked deliberately: `RenderContext<'a>` borrows `&'a Mpv`, and Slint's
    /// rendering notifier must be `'static`. The handle lives for the whole
    /// process anyway.
    mpv: &'static Mpv,
}

impl Player {
    pub fn new(hwdec: &str, verbose: bool) -> Result<Self> {
        // libmpv refuses to initialize unless the numeric locale is "C".
        unsafe { std::env::set_var("LC_NUMERIC", "C") };

        let mpv = Mpv::with_initializer(|init| {
            // vo=libmpv is what enables the render API.
            init.set_property("vo", "libmpv")?;
            init.set_property("hwdec", hwdec)?;
            // Frame-exact seeking: the whole point of the tool.
            init.set_property("hr-seek", "yes")?;
            // Hold the last frame instead of closing the file at EOF.
            init.set_property("keep-open", "yes")?;
            init.set_property("osc", false)?;
            init.set_property("input-default-bindings", false)?;
            if verbose {
                init.set_property("terminal", true)?;
                init.set_property("msg-level", "all=v")?;
            } else {
                init.set_property("terminal", false)?;
            }
            Ok(())
        })
        .map_err(|e| anyhow!("mpv initialization failed: {e:?}"))?;

        Ok(Self {
            mpv: Box::leak(Box::new(mpv)),
        })
    }

    /// The raw handle, needed to create the render context.
    pub fn mpv(&self) -> &'static Mpv {
        self.mpv
    }

    /// Load a file for playback.
    ///
    /// Must not be called before the render context exists: mpv initializes the
    /// video output at load time, and without one it fails and *permanently
    /// deselects the video track*.
    pub fn load(&self, path: &Path) -> Result<()> {
        self.mpv
            .command("loadfile", &[&path.display().to_string()])
            .map_err(|e| anyhow!("could not load {}: {e:?}", path.display()))
    }

    /// Seek to an absolute position, landing on the exact frame.
    pub fn seek(&self, seconds: f64) {
        let _ = self
            .mpv
            .command("seek", &[&format!("{seconds:.4}"), "absolute+exact"]);
    }

    /// Step one frame forward (`dir >= 0`) or back.
    pub fn frame_step(&self, dir: i32) {
        let cmd = if dir < 0 {
            "frame-back-step"
        } else {
            "frame-step"
        };
        let _ = self.mpv.command(cmd, &[]);
    }

    pub fn paused(&self) -> bool {
        self.mpv.get_property("pause").unwrap_or(true)
    }

    pub fn set_paused(&self, paused: bool) {
        let _ = self.mpv.set_property("pause", paused);
    }

    /// mpv's scale: 0-130, where 100 is unattenuated.
    pub fn set_volume(&self, volume: f64) {
        let _ = self.mpv.set_property("volume", volume.clamp(0.0, 130.0));
    }

    pub fn set_muted(&self, muted: bool) {
        let _ = self.mpv.set_property("mute", muted);
    }

    pub fn duration(&self) -> Option<f64> {
        self.mpv.get_property("duration").ok()
    }

    /// Loop a span of the file, or clear the loop with `None`.
    ///
    /// Used while refining marks, so the selected region plays over and over
    /// without needing to seek back manually.
    pub fn set_ab_loop(&self, span: Option<(f64, f64)>) {
        match span {
            Some((a, b)) => {
                let _ = self.mpv.set_property("ab-loop-a", a);
                let _ = self.mpv.set_property("ab-loop-b", b);
            }
            None => {
                // mpv clears these with the string "no", not a number.
                let _ = self.mpv.set_property("ab-loop-a", "no");
                let _ = self.mpv.set_property("ab-loop-b", "no");
            }
        }
    }

    /// Unload the current file, releasing the handle so it can be deleted.
    ///
    /// Windows refuses to remove a file that is still open, so this must happen
    /// before any attempt to delete the source.
    pub fn unload(&self) {
        let _ = self.mpv.command("stop", &[]);
    }

    /// Start pumping mpv's event queue, reporting changes through `sink`.
    ///
    /// `sink` runs on a background thread and must marshal to the UI itself.
    pub fn spawn_event_loop<F>(&self, sink: F)
    where
        F: Fn(PlayerEvent) + Send + 'static,
    {
        let mpv = self.mpv;
        std::thread::Builder::new()
            .name("mpv-events".into())
            .spawn(move || {
                let _ = mpv.observe_property("time-pos", Format::Double, ID_TIME_POS);
                let _ = mpv.observe_property("duration", Format::Double, ID_DURATION);
                let _ = mpv.observe_property("pause", Format::Flag, ID_PAUSE);

                // Set when a seek is issued so PlaybackRestart can time it.
                let mut seek_at: Option<Instant> = None;

                loop {
                    let Some(event) = mpv.wait_event(0.5) else {
                        continue;
                    };
                    match event {
                        Ok(Event::PropertyChange {
                            reply_userdata,
                            change,
                            ..
                        }) => match (reply_userdata, change) {
                            (ID_TIME_POS, PropertyData::Double(v)) => {
                                sink(PlayerEvent::Position(v))
                            }
                            (ID_DURATION, PropertyData::Double(v)) => {
                                sink(PlayerEvent::Duration(v))
                            }
                            (ID_PAUSE, PropertyData::Flag(v)) => sink(PlayerEvent::Paused(v)),
                            _ => {}
                        },
                        Ok(Event::FileLoaded) => sink(PlayerEvent::FileLoaded),
                        Ok(Event::Seek) => seek_at = Some(Instant::now()),
                        Ok(Event::PlaybackRestart) => {
                            if let Some(started) = seek_at.take() {
                                sink(PlayerEvent::SeekCompleted(started.elapsed()));
                            }
                        }
                        Ok(Event::EndFile(_)) => sink(PlayerEvent::EndOfFile),
                        Ok(Event::Shutdown) => {
                            sink(PlayerEvent::Shutdown);
                            break;
                        }
                        Ok(_) => {}
                        Err(e) => eprintln!("mpv event error: {e:?}"),
                    }
                }
            })
            .expect("spawn mpv event loop");
    }
}
