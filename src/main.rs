//! ClipCut — a fast video clipping tool.

// Release builds are GUI applications: launching from Explorer must not flash a
// console. Debug builds keep it, so the headless self-checks can report.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod render;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::{Result, anyhow};
use slint::{ComponentHandle, Model, RenderingState};

use clipcut::config::{ConfigStore, Settings};
use clipcut::encode::{AudioHandling, Container, CutMode, ExportRequest, Speed, VideoEncoder};
use clipcut::export::{self, ExportEvent, ExportHandle};
use clipcut::library::{self, LibraryWatcher, ScanQueue, VideoFile};
use clipcut::marks::Marks;
use clipcut::media::{SnapProbe, SnapResult};
use clipcut::player::{Player, PlayerEvent};
use clipcut::timecode;

use render::VideoBridge;

slint::include_modules!();

/// Option lists shown in the encoding panel, paired with the values they map to.
const ENCODERS: [(&str, VideoEncoder); 4] = [
    ("H.264 (x264, software)", VideoEncoder::X264),
    ("H.264 (NVENC)", VideoEncoder::NvencH264),
    ("HEVC (NVENC)", VideoEncoder::NvencHevc),
    ("AV1 (NVENC)", VideoEncoder::NvencAv1),
];
const SPEEDS: [(&str, Speed); 5] = [
    ("Fastest", Speed::Fastest),
    ("Fast", Speed::Fast),
    ("Balanced", Speed::Balanced),
    ("Slow", Speed::Slow),
    ("Slowest (best quality)", Speed::Slowest),
];
const CONTAINERS: [(&str, Container); 3] = [
    ("MP4", Container::Mp4),
    ("MKV", Container::Mkv),
    ("MP3 (audio only)", Container::Mp3),
];
const AUDIO_MODES: [(&str, AudioHandling); 3] = [
    ("AAC 192 kbps", AudioHandling::Aac { kbps: 192 }),
    ("AAC 320 kbps", AudioHandling::Aac { kbps: 320 }),
    ("Copy original", AudioHandling::Copy),
];

/// Convert a scanned file into a row for the list.
///
/// All formatting happens here so the UI never has to know about timestamps.
fn to_entry(file: &VideoFile) -> FileEntry {
    FileEntry {
        display_name: file.display_name.as_str().into(),
        sub_path: file.sub_path.as_str().into(),
        modified: library::format_modified(file.modified).into(),
        // Populated once durations are probed, in a later phase.
        duration: Default::default(),
        path: file.path.display().to_string().into(),
    }
}

fn index_of<T: PartialEq>(list: &[(&str, T)], value: &T) -> i32 {
    list.iter().position(|(_, v)| v == value).unwrap_or(0) as i32
}

fn labels(list: &[(&str, impl Sized)]) -> slint::ModelRc<slint::SharedString> {
    let items: Vec<slint::SharedString> = list.iter().map(|(l, _)| (*l).into()).collect();
    slint::ModelRc::new(slint::VecModel::from(items))
}

/// Everything the UI mutates, in one place so persistence has a single owner.
struct App {
    settings: Settings,
    store: ConfigStore,
    /// Kept alive to keep watching; replaced when the input folder changes.
    /// The file list itself lives in the Slint model, which already carries the
    /// path of each row, so there is no second copy to keep in sync.
    watcher: Option<LibraryWatcher>,
    /// The file currently loaded in the player.
    current: Option<PathBuf>,
    /// Marks per file, so switching away and back does not lose work.
    /// Deliberately not persisted: marks belong to an editing session.
    marks: HashMap<PathBuf, Marks>,
    /// The export in flight, if any. Held so it can be cancelled.
    export: Option<ExportHandle>,
    /// True while the timeline is zoomed into the selection.
    refining: bool,
}

impl App {
    fn marks_for(&self, path: &Path) -> Marks {
        self.marks.get(path).copied().unwrap_or_default()
    }
}

impl App {
    /// Apply a change and queue a save. Every setter goes through here so no
    /// mutation can silently skip persistence.
    fn update(&mut self, f: impl FnOnce(&mut Settings)) {
        f(&mut self.settings);
        self.store.store(&self.settings);
    }
}

fn main() -> Result<()> {
    let config_path = clipcut::config::default_path()
        .ok_or_else(|| anyhow!("could not determine a configuration directory"))?;
    let (settings, store) = ConfigStore::open_at(config_path);
    let app_state = Rc::new(RefCell::new(App {
        settings,
        store,
        watcher: None,
        current: None,
        marks: HashMap::new(),
        export: None,
        refining: false,
    }));

    let hwdec = std::env::var("CLIPCUT_HWDEC").unwrap_or_else(|_| "auto-safe".into());
    let verbose = std::env::var("CLIPCUT_MPV_VERBOSE").is_ok_and(|v| !v.is_empty());
    let player: &'static Player = Box::leak(Box::new(Player::new(&hwdec, verbose)?));

    let ui = AppWindow::new()?;

    // Restore persisted state before anything can overwrite it.
    {
        let st = app_state.borrow();
        ui.set_volume(st.settings.volume as f32);
        ui.set_muted(st.settings.muted);
        ui.set_encoding_expanded(st.settings.encoding_panel_expanded);
        ui.set_output_expanded(st.settings.output_panel_expanded);
        ui.set_input_folder(
            st.settings
                .input_folder
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
                .into(),
        );
        ui.set_output_folder(
            st.settings
                .output_folder
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
                .into(),
        );
        ui.window().set_size(slint::PhysicalSize::new(
            st.settings.window.width,
            st.settings.window.height,
        ));
        if let Some((x, y)) = st.settings.window.usable_position() {
            ui.window().set_position(slint::PhysicalPosition::new(x, y));
        }
    }

    ui.set_encoders(labels(&ENCODERS));
    ui.set_speeds(labels(&SPEEDS));
    ui.set_containers(labels(&CONTAINERS));
    ui.set_audio_modes(labels(&AUDIO_MODES));
    ui.set_version(env!("CARGO_PKG_VERSION").into());
    // Headless self-check: open the attribution dialog for inspection.
    ui.set_about_open(std::env::var("CLIPCUT_ABOUT").is_ok_and(|v| !v.is_empty()));
    sync_encoding_to_ui(&ui, &app_state.borrow().settings);

    // ---- keyframe snapping --------------------------------------------------
    // In Fast mode a stream copy cannot start mid-GOP, so it lands on the
    // preceding keyframe. Probing that in the background lets the timeline show
    // where the cut will really begin, instead of springing it on the user at
    // export time.
    let tools = clipcut::media::Tools::discover(app_state.borrow().settings.ffmpeg_path.as_deref());
    match &tools {
        Ok(t) => println!("FFMPEG {}", t.ffmpeg.display()),
        Err(e) => {
            eprintln!("warning: {e}");
            ui.set_encoding_warning(format!("ffmpeg not found: {e}").into());
        }
    }

    // ---- encoder availability ----------------------------------------------
    // Probing runs a one-frame encode per hardware encoder, which needs a real
    // driver and card, so it happens on a worker thread rather than delaying
    // startup. Until it reports, everything is assumed available.
    if let Ok(t) = &tools {
        let t = t.clone();
        let weak = ui.as_weak();
        std::thread::Builder::new()
            .name("encoder-probe".into())
            .spawn(move || {
                let candidates: Vec<&str> = ENCODERS.iter().map(|(_, e)| e.ffmpeg_name()).collect();
                let usable = t.usable_encoders(&candidates);
                let available: Vec<bool> = ENCODERS
                    .iter()
                    .map(|(_, e)| usable.contains(e.ffmpeg_name()))
                    .collect();
                println!(
                    "ENCODERS usable: {}",
                    ENCODERS
                        .iter()
                        .zip(&available)
                        .filter(|(_, ok)| **ok)
                        .map(|((_, e), _)| e.ffmpeg_name())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    let labels: Vec<slint::SharedString> = ENCODERS
                        .iter()
                        .zip(&available)
                        .map(|((label, _), ok)| {
                            if *ok {
                                (*label).into()
                            } else {
                                format!("{label} — unavailable").into()
                            }
                        })
                        .collect();
                    ui.set_encoders(slint::ModelRc::new(slint::VecModel::from(labels)));
                    ui.set_encoder_available(slint::ModelRc::new(slint::VecModel::from(available)));
                    refresh_encoder_warning(&ui);
                });
            })
            .expect("spawn encoder probe");
    }

    let snap_probe = tools.as_ref().ok().map(|t| {
        Rc::new(SnapProbe::new(t.clone(), {
            let weak = ui.as_weak();
            move |result: SnapResult| {
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    // Discard results for a file or mark we have already moved on from.
                    if ui.get_current_file().as_str() != result.file.to_string_lossy() {
                        return;
                    }
                    if (ui.get_mark_in() as f64 - result.mark).abs() > 1e-3 {
                        return;
                    }
                    match result.snapped {
                        Some(at) => {
                            ui.set_ghost_in(at as f32);
                            let behind = result.mark - at;
                            ui.set_snap_note(
                                if behind < 0.05 {
                                    // The mark is already on a keyframe: nothing is lost.
                                    "Fast cut starts exactly on the in point".to_string()
                                } else {
                                    format!(
                                        "Fast cut starts at {} — {behind:.1}s before the in point",
                                        timecode::clock_precise(at)
                                    )
                                }
                                .into(),
                            );
                        }
                        None => {
                            ui.set_ghost_in(-1.0);
                            ui.set_snap_note("".into());
                        }
                    }
                });
            }
        }))
    });

    // Ask for a snap position, or clear it when the question does not apply.
    let refresh_snap = {
        let state = app_state.clone();
        let weak = ui.as_weak();
        let probe = snap_probe.clone();
        Rc::new(move || {
            let Some(ui) = weak.upgrade() else { return };
            let precise = state.borrow().settings.encoding.mode == CutMode::Precise;
            let marks = current_marks(&state);

            // Precise mode honours the mark exactly, so there is nothing to warn about.
            match (precise, marks.in_point, &probe) {
                (false, Some(mark), Some(probe)) => {
                    if let Some(path) = state.borrow().current.clone() {
                        probe.request(path, mark);
                    }
                }
                _ => {
                    ui.set_ghost_in(-1.0);
                    ui.set_snap_note("".into());
                }
            }
        })
    };

    // ---- render bridge -----------------------------------------------------
    // The file is loaded from here, not before run(): mpv initializes the video
    // output at load time and, with no render context, permanently deselects the
    // video track.
    let pending_load: Rc<RefCell<Option<PathBuf>>> =
        Rc::new(RefCell::new(std::env::args().nth(1).map(PathBuf::from)));

    let weak = ui.as_weak();
    let mut bridge: Option<VideoBridge> = None;
    let load_on_setup = pending_load.clone();

    ui.window()
        .set_rendering_notifier(move |state, api| match state {
            RenderingState::RenderingSetup => match VideoBridge::new(player.mpv(), api) {
                Ok(mut b) => {
                    let w = weak.clone();
                    b.on_new_frame(move || {
                        let _ = w.upgrade_in_event_loop(|ui| ui.window().request_redraw());
                    });
                    bridge = Some(b);

                    if let Some(path) = load_on_setup.borrow_mut().take()
                        && let Err(e) = player.load(&path)
                    {
                        eprintln!("error: {e}");
                    }
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    if let Some(ui) = weak.upgrade() {
                        ui.set_video_status(format!("Video unavailable: {e}").into());
                    }
                }
            },

            RenderingState::BeforeRendering => {
                if let (Some(b), Some(ui)) = (bridge.as_mut(), weak.upgrade()) {
                    let scale = ui.window().scale_factor();
                    let w = (ui.get_viewport_width() * scale).round().max(16.0) as u32;
                    let h = (ui.get_viewport_height() * scale).round().max(16.0) as u32;
                    if let Some(frame) = b.render(api, w, h) {
                        ui.set_video_frame(frame);
                    }
                    maybe_dump(b);
                }
            }

            // Capture the composited window, which is what the user sees.
            RenderingState::AfterRendering => {
                if let (Some(b), Some(ui)) = (bridge.as_ref(), weak.upgrade()) {
                    maybe_dump_window(b, &ui);
                }
            }

            // The render context must be dropped while the GL context is current.
            RenderingState::RenderingTeardown => bridge = None,

            _ => {}
        })
        .map_err(|e| anyhow!("could not install the rendering notifier: {e:?}"))?;

    // ---- source library -----------------------------------------------------
    // Scans run on a worker thread and are coalesced, so a burst of filesystem
    // events cannot pile up behind a slow scan of a large tree.
    let scan_queue = Rc::new(ScanQueue::new({
        let weak = ui.as_weak();
        move |_root, files: Vec<VideoFile>| {
            let _ = weak.upgrade_in_event_loop(move |ui| {
                let entries: Vec<FileEntry> = files.iter().map(to_entry).collect();
                ui.set_files(slint::ModelRc::new(slint::VecModel::from(entries)));

                // Follow the selected *file*, not its row number. Rows shift
                // whenever a file is added or removed, so tracking the index
                // would silently move the selection to a different video.
                let current = ui.get_current_file();
                let index = if current.is_empty() {
                    None
                } else {
                    files
                        .iter()
                        .position(|f| f.path.to_string_lossy() == current.as_str())
                };
                ui.set_selected_index(index.map(|i| i as i32).unwrap_or(-1));
                ui.set_library_summary(
                    if files.is_empty() {
                        String::new()
                    } else {
                        let total: u64 = files.iter().map(|f| f.size).sum();
                        format!(
                            "{} file{} · {}",
                            files.len(),
                            if files.len() == 1 { "" } else { "s" },
                            library::format_size(total)
                        )
                    }
                    .into(),
                );
                ui.set_scanning(false);
                println!("SCAN_DONE {}", files.len());

                // Headless self-check: exercise the list -> select -> load path
                // without a human clicking a row.
                if let Ok(n) = std::env::var("CLIPCUT_AUTOSELECT")
                    && let Ok(n) = n.parse::<i32>()
                    && n >= 0
                    && n < files.len() as i32
                    && ui.get_selected_index() < 0
                {
                    ui.invoke_select_file(n);
                }
            });
        }
    }));

    // Point the app at a folder: persist it, scan it, and watch it.
    let use_folder = {
        let state = app_state.clone();
        let queue = scan_queue.clone();
        let weak = ui.as_weak();
        Rc::new(move |root: PathBuf| {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_input_folder(root.display().to_string().into());
            ui.set_scanning(true);
            state
                .borrow_mut()
                .update(|s| s.input_folder = Some(root.clone()));

            queue.request(root.clone());

            let handle = queue.handle();
            let watched = root.clone();
            let watcher = library::watch(&root, move || handle.request(watched.clone()));
            match watcher {
                Ok(w) => state.borrow_mut().watcher = Some(w),
                Err(e) => {
                    // Watching is a convenience; scanning still works without it.
                    eprintln!("warning: cannot watch {}: {e}", root.display());
                    state.borrow_mut().watcher = None;
                }
            }
        })
    };

    ui.on_choose_input_folder({
        let use_folder = use_folder.clone();
        let state = app_state.clone();
        move || {
            let start = state.borrow().settings.input_folder.clone();
            let mut dialog = rfd::FileDialog::new().set_title("Choose the folder with your videos");
            if let Some(dir) = start.filter(|p| p.is_dir()) {
                dialog = dialog.set_directory(dir);
            }
            if let Some(folder) = dialog.pick_folder() {
                use_folder(folder);
            }
        }
    });

    ui.on_select_file({
        let weak = ui.as_weak();
        let state = app_state.clone();
        let refresh = refresh_snap.clone();
        move |index| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(entry) = ui.get_files().row_data(index as usize) else {
                return;
            };
            let path = PathBuf::from(entry.path.as_str());

            ui.set_selected_index(index);
            ui.set_has_file(false);
            ui.set_video_status("Loading…".into());

            // Restore whatever marks this file already had, so switching away
            // and back does not lose a selection in progress.
            let marks = {
                let mut st = state.borrow_mut();
                st.current = Some(path.clone());
                st.marks_for(&path)
            };
            show_marks(&ui, marks);
            ui.set_output_filename(suggest_filename(&path).into());
            ui.set_current_file(entry.path.clone());
            refresh();

            if let Err(e) = player.load(&path) {
                eprintln!("error: {e}");
                ui.set_video_status(format!("Could not open this file: {e}").into());
            }
        }
    });

    // ---- playback callbacks -------------------------------------------------
    ui.on_toggle_play(move || player.set_paused(!player.paused()));
    ui.on_step(move |dir| player.frame_step(dir));
    ui.on_seek(move |secs| player.seek(secs as f64));

    ui.on_toggle_mute({
        let state = app_state.clone();
        let weak = ui.as_weak();
        move || {
            let muted = {
                let mut st = state.borrow_mut();
                let next = !st.settings.muted;
                st.update(|s| s.muted = next);
                next
            };
            player.set_muted(muted);
            if let Some(ui) = weak.upgrade() {
                ui.set_muted(muted);
            }
        }
    });

    ui.on_set_volume({
        let state = app_state.clone();
        let weak = ui.as_weak();
        move |v| {
            let volume = (v as f64).clamp(0.0, 130.0);
            state.borrow_mut().update(|s| s.volume = volume);
            player.set_volume(volume);
            if let Some(ui) = weak.upgrade() {
                ui.set_volume(volume as f32);
                // Touching the slider is an unambiguous intent to hear something.
                if volume > 0.0 && ui.get_muted() {
                    ui.set_muted(false);
                    player.set_muted(false);
                    state.borrow_mut().update(|s| s.muted = false);
                }
            }
        }
    });

    // ---- marks --------------------------------------------------------------
    // Every mark change funnels through here so the timeline, labels and export
    // gating can never disagree with the stored marks.
    let apply_marks = {
        let state = app_state.clone();
        let weak = ui.as_weak();
        let refresh = refresh_snap.clone();
        // Args are (marks, current position, file duration).
        Rc::new(move |edit: &dyn Fn(&mut Marks, f64, f64)| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(path) = state.borrow().current.clone() else {
                return;
            };
            let duration = ui.get_duration() as f64;
            let position = ui.get_position() as f64;

            let mut marks = state.borrow().marks_for(&path);
            edit(&mut marks, position, duration);
            state.borrow_mut().marks.insert(path, marks);
            show_marks(&ui, marks);
            refresh();

            // Keep the loop in step while marks are nudged during refinement.
            if state.borrow().refining {
                match marks.selection() {
                    Some((start, length)) => player.set_ab_loop(Some((start, start + length))),
                    None => {
                        state.borrow_mut().refining = false;
                        player.set_ab_loop(None);
                    }
                }
            }
        })
    };

    ui.on_set_mark_in({
        let apply = apply_marks.clone();
        move || apply(&|m, at, dur| m.set_in(at, dur))
    });
    ui.on_set_mark_out({
        let apply = apply_marks.clone();
        move || apply(&|m, at, dur| m.set_out(at, dur))
    });
    ui.on_clear_marks({
        let apply = apply_marks.clone();
        move || apply(&|m, _, _| m.clear())
    });

    ui.on_goto_mark({
        let state = app_state.clone();
        move |which| {
            let Some(path) = state.borrow().current.clone() else {
                return;
            };
            let marks = state.borrow().marks_for(&path);
            let target = if which == 0 {
                marks.in_point
            } else {
                marks.out_point
            };
            if let Some(t) = target {
                player.seek(t);
            }
        }
    });

    // ---- delete a source file -----------------------------------------------
    // Only ever the selected file, because mpv has it open and the handle must
    // be released before the filesystem will let go of it. Goes to the recycle
    // bin rather than being unlinked: a source recording is often the only copy,
    // and a misclick should be recoverable.
    ui.on_delete_file({
        let state = app_state.clone();
        let weak = ui.as_weak();
        let queue = scan_queue.clone();
        move |index| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(entry) = ui.get_files().row_data(index as usize) else {
                return;
            };
            if index != ui.get_selected_index() {
                return;
            }
            let path = PathBuf::from(entry.path.as_str());

            // Release the file before touching it.
            player.unload();
            {
                let mut st = state.borrow_mut();
                st.current = None;
                st.marks.remove(&path);
                st.refining = false;
            }
            ui.set_has_file(false);
            ui.set_current_file("".into());
            ui.set_selected_index(-1);
            ui.set_refining(false);
            ui.set_can_refine(false);
            ui.set_view_end(-1.0);
            player.set_ab_loop(None);
            show_marks(&ui, Marks::default());

            match trash::delete(&path) {
                Ok(()) => {
                    ui.set_video_status(
                        format!(
                            "Moved {} to the recycle bin",
                            path.file_name().unwrap_or_default().to_string_lossy()
                        )
                        .into(),
                    );
                    println!("DELETED {}", path.display());
                }
                Err(e) => {
                    eprintln!("error: could not delete {}: {e}", path.display());
                    ui.set_video_status(format!("Could not delete: {e}").into());
                }
            }

            // Rescan so the row disappears even if the watcher is slow or absent.
            if let Some(root) = state.borrow().settings.input_folder.clone() {
                queue.request(root);
            }
        }
    });

    // ---- refine -------------------------------------------------------------
    // Zooms the timeline into the selection and loops it, so marks can be nudged
    // precisely on a file where the whole clip is otherwise a few pixels wide.
    ui.on_toggle_refine({
        let state = app_state.clone();
        let weak = ui.as_weak();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let marks = current_marks(&state);
            let active = !state.borrow().refining;

            match (active, marks.selection()) {
                (true, Some((start, length))) => {
                    // A little context either side, so the marks are reachable
                    // rather than pinned to the very edges of the track.
                    let pad = (length * 0.15).clamp(0.25, 5.0);
                    let duration = ui.get_duration() as f64;
                    let view_start = (start - pad).max(0.0);
                    let view_end = if duration > 0.0 {
                        (start + length + pad).min(duration)
                    } else {
                        start + length + pad
                    };

                    state.borrow_mut().refining = true;
                    ui.set_refining(true);
                    ui.set_view_start(view_start as f32);
                    ui.set_view_end(view_end as f32);
                    player.set_ab_loop(Some((start, start + length)));
                    player.seek(start);
                    player.set_paused(false);
                }
                _ => {
                    state.borrow_mut().refining = false;
                    ui.set_refining(false);
                    ui.set_view_start(0.0);
                    ui.set_view_end(-1.0);
                    player.set_ab_loop(None);
                }
            }
        }
    });

    // ---- output destination -------------------------------------------------
    ui.on_choose_output_folder({
        let state = app_state.clone();
        let weak = ui.as_weak();
        move || {
            let start = state.borrow().settings.output_folder.clone();
            let mut dialog = rfd::FileDialog::new().set_title("Choose where to save clips");
            if let Some(dir) = start.filter(|p| p.is_dir()) {
                dialog = dialog.set_directory(dir);
            }
            if let Some(folder) = dialog.pick_folder() {
                let Some(ui) = weak.upgrade() else { return };
                ui.set_output_folder(folder.display().to_string().into());
                state
                    .borrow_mut()
                    .update(|s| s.output_folder = Some(folder));
                // Choosing a folder can be what unblocks exporting.
                let marks = current_marks(&state);
                show_marks(&ui, marks);
            }
        }
    });

    ui.on_set_filename({
        let weak = ui.as_weak();
        move |name| {
            if let Some(ui) = weak.upgrade() {
                ui.set_output_filename(name);
            }
        }
    });

    // ---- export -------------------------------------------------------------
    ui.on_export({
        let state = app_state.clone();
        let weak = ui.as_weak();
        let tools = tools.as_ref().ok().cloned();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let Some(tools) = tools.clone() else {
                ui.set_export_status("ffmpeg was not found — cannot export".into());
                return;
            };

            let (source, marks, settings) = {
                let st = state.borrow();
                let Some(source) = st.current.clone() else {
                    return;
                };
                let marks = st.marks_for(&source);
                (source, marks, st.settings.clone())
            };
            let Some((start, duration)) = marks.selection() else {
                return;
            };
            let Some(out_dir) = settings.output_folder.clone() else {
                ui.set_export_status("Choose an output folder first".into());
                return;
            };

            // Never overwrite: the source of a clip is often the only copy.
            let output = export::unique_output_path(
                &out_dir,
                ui.get_output_filename().as_str(),
                settings.encoding.container.extension(),
            );

            let request = ExportRequest {
                input: source,
                output: output.clone(),
                start,
                duration,
                audio_track: None,
                settings: settings.encoding.clone(),
            };

            ui.set_exporting(true);
            ui.set_can_export(false);
            ui.set_export_progress(0.0);
            ui.set_export_detail("Starting…".into());

            let sink_weak = ui.as_weak();
            let result = export::start(&tools, request, move |event| {
                let _ = sink_weak.upgrade_in_event_loop(move |ui| match event {
                    ExportEvent::Progress(p) => {
                        let clip = (ui.get_mark_out() - ui.get_mark_in()) as f64;
                        ui.set_export_progress(p.fraction(clip) as f32);
                        ui.set_export_detail(
                            match p.eta_secs(clip) {
                                Some(eta) => format!(
                                    "{} of {}  ·  {:.1}x  ·  about {} left",
                                    timecode::clock(p.out_time.as_secs_f64()),
                                    timecode::clock(clip),
                                    p.speed,
                                    timecode::clock(eta)
                                ),
                                None => format!(
                                    "{} of {}",
                                    timecode::clock(p.out_time.as_secs_f64()),
                                    timecode::clock(clip)
                                ),
                            }
                            .into(),
                        );
                    }
                    ExportEvent::Finished { output } => {
                        ui.set_exporting(false);
                        ui.set_export_progress(1.0);
                        ui.set_export_status(
                            format!(
                                "Saved {}",
                                output
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default()
                            )
                            .into(),
                        );
                        ui.set_can_export(true);
                        // Deliberately does not open a file manager: exporting
                        // several clips in a row would bury you in windows. The
                        // status line names the file and the panel shows the folder.
                        println!("EXPORT_DONE {}", output.display());
                    }
                    ExportEvent::Failed { message } => {
                        ui.set_exporting(false);
                        // ffmpeg's own words, not a generic failure.
                        ui.set_export_status(format!("Export failed: {message}").into());
                        ui.set_can_export(true);
                        println!("EXPORT_FAILED {message}");
                    }
                    ExportEvent::Cancelled => {
                        ui.set_exporting(false);
                        ui.set_export_status("Export cancelled".into());
                        ui.set_can_export(true);
                        println!("EXPORT_CANCELLED");
                    }
                });
            });

            match result {
                Ok(handle) => state.borrow_mut().export = Some(handle),
                Err(e) => {
                    ui.set_exporting(false);
                    ui.set_can_export(true);
                    ui.set_export_status(format!("Could not start ffmpeg: {e}").into());
                }
            }
        }
    });

    ui.on_cancel_export({
        let state = app_state.clone();
        move || {
            if let Some(handle) = state.borrow().export.as_ref() {
                handle.cancel();
            }
        }
    });

    // ---- timeline hover -----------------------------------------------------
    ui.on_hover_time({
        let weak = ui.as_weak();
        move |t| {
            if let Some(ui) = weak.upgrade() {
                ui.set_hover_label(timecode::clock_precise(t as f64).into());
            }
        }
    });
    ui.on_hover_exit({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_hover_label("".into());
            }
        }
    });

    // ---- panel expansion ----------------------------------------------------
    ui.on_toggle_encoding({
        let state = app_state.clone();
        move |expanded| {
            state
                .borrow_mut()
                .update(|s| s.encoding_panel_expanded = expanded)
        }
    });
    ui.on_toggle_output({
        let state = app_state.clone();
        move |expanded| {
            state
                .borrow_mut()
                .update(|s| s.output_panel_expanded = expanded)
        }
    });

    // ---- encoding settings --------------------------------------------------
    macro_rules! on_encoding_change {
        ($setter:ident, $arg:ident, $body:expr) => {{
            let state = app_state.clone();
            let weak = ui.as_weak();
            let refresh = refresh_snap.clone();
            ui.$setter(move |$arg| {
                {
                    let mut st = state.borrow_mut();
                    st.update(|s| $body(&mut s.encoding));
                }
                if let Some(ui) = weak.upgrade() {
                    sync_encoding_to_ui(&ui, &state.borrow().settings);
                    refresh();
                }
            });
        }};
    }

    on_encoding_change!(
        on_set_mode,
        precise,
        |e: &mut clipcut::encode::EncodeSettings| {
            e.mode = if precise {
                CutMode::Precise
            } else {
                CutMode::Fast
            };
        }
    );
    on_encoding_change!(
        on_set_encoder,
        i,
        |e: &mut clipcut::encode::EncodeSettings| {
            if let Some((_, v)) = ENCODERS.get(i as usize) {
                e.video = *v;
                // The quality scale differs per encoder, so keep it in range.
                let (lo, hi, _) = v.quality_range();
                e.quality = e.quality.clamp(lo, hi);
            }
        }
    );
    on_encoding_change!(
        on_set_speed,
        i,
        |e: &mut clipcut::encode::EncodeSettings| {
            if let Some((_, v)) = SPEEDS.get(i as usize) {
                e.speed = *v;
            }
        }
    );
    on_encoding_change!(
        on_set_container,
        i,
        |e: &mut clipcut::encode::EncodeSettings| {
            if let Some((_, v)) = CONTAINERS.get(i as usize) {
                e.container = *v;
            }
        }
    );
    on_encoding_change!(
        on_set_audio,
        i,
        |e: &mut clipcut::encode::EncodeSettings| {
            if let Some((_, v)) = AUDIO_MODES.get(i as usize) {
                e.audio = *v;
            }
        }
    );
    on_encoding_change!(
        on_set_quality,
        q,
        |e: &mut clipcut::encode::EncodeSettings| {
            let (lo, hi, _) = e.video.quality_range();
            e.quality = (q as u32).clamp(lo, hi);
        }
    );

    // Scan the remembered folder on startup.
    // The clone is bound first so the shared borrow ends before `use_folder`
    // takes a mutable one.
    let startup_folder = app_state.borrow().settings.input_folder.clone();
    if let Some(root) = startup_folder.filter(|p| p.is_dir()) {
        use_folder(root);
    }

    // ---- mpv events ---------------------------------------------------------
    {
        let weak = ui.as_weak();
        player.spawn_event_loop(move |event| {
            // Marshal to the UI thread: the settings live behind an Rc and the
            // UI properties already mirror them, so nothing needs sharing here.
            let _ = weak.upgrade_in_event_loop(move |ui| match event {
                PlayerEvent::Position(t) => {
                    ui.set_position(t as f32);
                    ui.set_position_label(timecode::clock(t).into());
                }
                PlayerEvent::Duration(d) => {
                    ui.set_duration(d as f32);
                    ui.set_duration_label(timecode::clock(d).into());
                    // Headless self-check: place marks so the timeline can be
                    // inspected without a human pressing I and O.
                    if let Ok(spec) = std::env::var("CLIPCUT_AUTOMARK") {
                        let mut parts =
                            spec.split(',').filter_map(|p| p.trim().parse::<f32>().ok());
                        if let (Some(a), Some(b)) = (parts.next(), parts.next()) {
                            ui.set_position(a);
                            ui.invoke_set_mark_in();
                            ui.set_position(b);
                            ui.invoke_set_mark_out();
                            // Headless self-check: enter refine mode.
                            if std::env::var_os("CLIPCUT_AUTOREFINE").is_some() {
                                ui.invoke_toggle_refine();
                            }
                            // Headless self-check: run a real export end to end.
                            if std::env::var_os("CLIPCUT_AUTOEXPORT").is_some() {
                                ui.invoke_export();
                            }
                        }
                    }
                }
                PlayerEvent::Paused(p) => ui.set_playing(!p),
                PlayerEvent::FileLoaded => {
                    ui.set_has_file(true);
                    ui.set_video_status("".into());
                    // Apply the restored audio state now that mpv has a file.
                    player.set_volume(ui.get_volume() as f64);
                    player.set_muted(ui.get_muted());
                }
                PlayerEvent::SeekCompleted(_) | PlayerEvent::EndOfFile => {}
                PlayerEvent::Shutdown => {}
            });
        });
    }

    // ---- persist window size on exit ---------------------------------------
    let result = ui.run();
    {
        let size = ui.window().size();
        let mut st = app_state.borrow_mut();
        st.settings.window.width = size.width;
        st.settings.window.height = size.height;
        let pos = ui.window().position();
        st.settings.window.x = Some(pos.x);
        st.settings.window.y = Some(pos.y);
        // Synchronous: UI callbacks still hold Rc clones, so the store's Drop
        // would not run here and a queued write would be lost.
        st.store.save_now(&st.settings);
    }
    result.map_err(Into::into)
}

/// Marks for whichever file is loaded, or empty marks if none is.
fn current_marks(state: &Rc<RefCell<App>>) -> Marks {
    let st = state.borrow();
    st.current
        .as_ref()
        .map(|p| st.marks_for(p))
        .unwrap_or_default()
}

/// Suggest an output name from the source, e.g. `Session 2026-08-14_001`.
///
/// The counter is a starting point only; collision handling happens at export,
/// once the output folder and container are both known.
fn suggest_filename(source: &Path) -> String {
    let stem = source
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "clip".to_string());
    format!("{stem}_001")
}

/// Push marks into the UI: timeline positions, the selection readout, and
/// whether an export is possible.
fn show_marks(ui: &AppWindow, marks: Marks) {
    ui.set_mark_in(marks.in_point.map(|v| v as f32).unwrap_or(-1.0));
    ui.set_mark_out(marks.out_point.map(|v| v as f32).unwrap_or(-1.0));

    match marks.selection() {
        Some((start, length)) => {
            ui.set_selection_label(
                format!(
                    "{} → {}  ({})",
                    timecode::clock_precise(start),
                    timecode::clock_precise(start + length),
                    timecode::clock_precise(length)
                )
                .into(),
            );
        }
        None => ui.set_selection_label("".into()),
    }

    let ready = marks.is_exportable();
    // Refining only means anything once there is a span to zoom into.
    ui.set_can_refine(ready);
    if !ready && ui.get_refining() {
        // The selection that was being refined no longer exists.
        ui.set_refining(false);
        ui.set_view_start(0.0);
        ui.set_view_end(-1.0);
    }
    // Only a re-encode actually runs the chosen video encoder; a stream copy or
    // an audio-only export does not care whether it exists.
    let encoder_ok = ui.get_audio_only() || !ui.get_precise() || ui.get_encoder_available_now();
    ui.set_can_export(
        ready && encoder_ok && !ui.get_output_folder().is_empty() && !ui.get_exporting(),
    );
    ui.set_export_status(match marks.missing() {
        Some(note) => note.into(),
        None if ui.get_output_folder().is_empty() => "Choose an output folder".into(),
        None => slint::SharedString::from("Ready to export"),
    });
}

/// Warn when the chosen encoder cannot run on this machine.
///
/// The common case is an NVENC encoder compiled into ffmpeg on a machine with no
/// NVIDIA card: it looks available until the export fails.
fn refresh_encoder_warning(ui: &AppWindow) {
    let index = ui.get_encoder_index();
    let available = ui.get_encoder_available();
    let ok = available
        .row_data(index as usize)
        // Assume available until the probe has reported.
        .unwrap_or(true);

    ui.set_encoder_available_now(ok);
    ui.set_encoding_warning(if ok {
        Default::default()
    } else {
        let needs_gpu = ENCODERS
            .get(index as usize)
            .is_some_and(|(_, e)| e.needs_nvidia());
        if needs_gpu {
            "This encoder needs an NVIDIA GPU that ffmpeg can reach. Choose another.".into()
        } else {
            "Your ffmpeg build does not provide this encoder. Choose another.".into()
        }
    });
}

/// Push encoding settings into the UI, including the derived labels.
fn sync_encoding_to_ui(ui: &AppWindow, settings: &Settings) {
    let e = &settings.encoding;
    let precise = e.mode == CutMode::Precise;
    let (lo, hi, _) = e.video.quality_range();

    ui.set_precise(precise);
    ui.set_audio_only(e.container.is_audio_only());
    refresh_encoder_warning(ui);
    ui.set_encoder_index(index_of(&ENCODERS, &e.video));
    ui.set_speed_index(index_of(&SPEEDS, &e.speed));
    ui.set_container_index(index_of(&CONTAINERS, &e.container));
    ui.set_audio_index(index_of(&AUDIO_MODES, &e.audio));
    ui.set_quality(e.quality as i32);
    ui.set_quality_label(e.video.quality_label().into());
    ui.set_quality_min(lo as i32);
    ui.set_quality_max(hi as i32);
    ui.set_output_extension(e.container.extension().into());

    ui.set_encoding_summary(
        if precise {
            format!(
                "{} · {} {}",
                ENCODERS[index_of(&ENCODERS, &e.video) as usize].0,
                e.video.quality_label(),
                e.quality
            )
        } else {
            "Stream copy".to_string()
        }
        .into(),
    );
}

/// Headless self-check: dump the whole composited window and quit.
///
/// This is the only way to inspect what the user actually sees — video *and*
/// Slint's widgets — without screen-capturing the desktop.
fn maybe_dump_window(bridge: &VideoBridge, ui: &AppWindow) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static FRAMES: AtomicU32 = AtomicU32::new(0);

    let Ok(path) = std::env::var("CLIPCUT_DUMP_WINDOW") else {
        return;
    };
    let after: u32 = std::env::var("CLIPCUT_DUMP_AFTER")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    if FRAMES.fetch_add(1, Ordering::Relaxed) + 1 < after {
        return;
    }

    let size = ui.window().size();
    let buf = bridge.read_window(size.width, size.height);
    match std::fs::write(&path, &buf) {
        Ok(()) => println!("WINDOW_OK {} {} {path}", size.width, size.height),
        Err(e) => println!("WINDOW_ERR {e}"),
    }
    let _ = slint::quit_event_loop();
}

/// Headless self-check: dump the mpv-rendered framebuffer and quit.
///
/// Lets the render path be verified without a human looking at the screen.
fn maybe_dump(bridge: &VideoBridge) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static FRAMES: AtomicU32 = AtomicU32::new(0);

    let Ok(path) = std::env::var("CLIPCUT_DUMP") else {
        return;
    };
    let after: u32 = std::env::var("CLIPCUT_DUMP_AFTER")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);

    if FRAMES.fetch_add(1, Ordering::Relaxed) + 1 < after {
        return;
    }
    match bridge.read_pixels() {
        Some((w, h, buf)) => match std::fs::write(&path, &buf) {
            Ok(()) => println!("DUMP_OK {w} {h} {path}"),
            Err(e) => println!("DUMP_ERR {e}"),
        },
        None => println!("DUMP_ERR no render target"),
    }
    let _ = slint::quit_event_loop();
}
