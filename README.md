# ClipCut

A fast video clipping tool: point it at a folder of recordings, scrub to find a cut
point, set in/out marks, export a clip.

Built on **Rust + [Slint](https://slint.dev) + [libmpv](https://mpv.io)**, with ffmpeg
invoked as a subprocess for encoding.

Point it at a folder of recordings and the file list fills up, oldest first, updating
live as new ones appear. Click one, scrub to find your cut point, set in and out marks,
and export. In stream-copy mode the timeline shows where the cut will *really* land,
since a copy cannot start mid-GOP. Every setting is saved the moment it changes.

Windows is the developed and tested platform. Nothing in the code is Windows-only —
libmpv, Slint and ffmpeg are all cross-platform — but the setup and packaging scripts are
PowerShell, and no other platform has been tried.

A file path may also be passed as a command-line argument, which loads it directly and
bypasses the list.

## Prerequisites

| Requirement | Notes |
|---|---|
| Rust, MSVC toolchain | `rustup default stable-x86_64-pc-windows-msvc` |
| ffmpeg + ffprobe on `PATH` | Used at runtime for encoding; not bundled |
| 7-Zip | First-time libmpv setup only |

## Build

```powershell
.\scripts\setup-mpv.ps1     # once: fetch libmpv, generate the MSVC import library
cargo build
```

That is the whole build. `setup-mpv.ps1` is idempotent, so re-running it is cheap; pass
`-Force` to re-download.

### Why setup-mpv.ps1 exists

This is the one genuinely awkward dependency. zhongfly's mpv-winbuild ships
`libmpv.dll.a`, a **MinGW** import library that the MSVC linker cannot use, and there is
no MSVC-format equivalent published. The script synthesises a COFF `mpv.lib` from the
DLL's export table with `dumpbin` + `lib`.

One detail matters: the generated `.def` must carry `LIBRARY libmpv-2.dll`. Without it,
`lib.exe` derives the DLL name from the `.def` filename, the executable records an import
against a non-existent `mpv.dll`, and it fails at startup with `STATUS_DLL_NOT_FOUND`.

It fetches the **LGPL** build deliberately. The GPL variant would make the whole binary
GPL on distribution, and nothing here needs the GPL-only parts — H.264/AV1 decoding and
the render API are all in the LGPL build.

## Running

```powershell
cargo run
cargo run -- path\to\video.mkv     # skip the file list and open one directly
```

`Space` play/pause · `←`/`→` ±5 s · `,`/`.` frame back/forward · `I`/`O` set in/out ·
`Home`/`End` jump to a mark · `M` mute

## Testing

```powershell
cargo test
```

Unit tests cover the pure logic: ffmpeg argument construction, `-progress` parsing
(including lines split at every possible byte boundary, as happens when reading from a
pipe), timecode handling, mark rules, keyframe selection and output naming.

Integration tests drive real ffmpeg processes — they generate their own fixtures, run
Fast and Precise exports, verify a cancelled export kills ffmpeg and leaves no partial
file, and check that the keyframe position the UI reports is where a stream copy actually
starts. They skip themselves when ffmpeg is unavailable.

## Packaging

```powershell
.\scripts\package.ps1
```

Produces `dist\clipcut-<version>\` and a zip beside it containing the executable,
`libmpv-2.dll` and the licence notices. Portable rather than an installer, deliberately:
no registry entries, no file associations, ffmpeg comes from `PATH`, and settings live in
`%APPDATA%`. Unzip-and-run is the whole install.

Release builds set `windows_subsystem = "windows"`, so launching from Explorer does not
flash a console. Debug builds keep it, so the headless self-checks below can report.

## Troubleshooting

**`linker 'link.exe' not found`** — the Visual Studio Build Tools C++ workload is not
registered, so `vcvarsall.bat` adds nothing to `PATH`. Fix it properly with:

```powershell
winget install --id Microsoft.VisualStudio.2026.BuildTools --override `
  "--quiet --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
```

If that is not an option, `.\scripts\build.ps1 <cargo args>` locates a usable MSVC
toolchain and Windows SDK itself and then runs cargo. Nothing in the dependency tree
compiles C, so only the libraries are needed, not the headers.

**`error: failed to remove file ... clipcut.exe`** — a running instance is holding the
binary. Close it and rebuild.

## Headless self-checks

The app can verify its own rendering without anyone looking at the screen, by reading
frames back off the GPU. These drove development of the render bridge and remain the way
to check it after changes.

```powershell
# Capture the whole composited window — video and UI — to raw RGBA, then quit
$env:CLIPCUT_DUMP_WINDOW = "win.raw"; $env:CLIPCUT_DUMP_AFTER = "300"
cargo run
ffmpeg -f rawvideo -pix_fmt rgba -s 1280x800 -i win.raw -vf vflip win.png

# Drive the app without clicking
$env:CLIPCUT_AUTOSELECT = "0"        # select row N once the scan completes
$env:CLIPCUT_AUTOMARK = "120,430"    # place in/out marks, in seconds
$env:CLIPCUT_AUTOEXPORT = "1"        # run a real export once marks are placed
$env:CLIPCUT_ABOUT = "1"             # open the About dialog

# Benchmark N frame-exact seeks scattered across a file
$env:CLIPCUT_SEEK_BENCH = "25"
cargo run -- video.mkv
```

`CLIPCUT_DUMP` captures mpv's own framebuffer instead of the window. Other toggles:
`CLIPCUT_MPV_VERBOSE=1` for mpv's log, `CLIPCUT_HWDEC=no` to rule out hardware decoding.

---

## Encoding

`src/encode/command.rs` turns settings + marks into an ffmpeg argv. It is a pure
function, so every export is reproducible by pasting the logged command into a
terminal. Inspect what it produces:

```powershell
cargo run --example show-commands
```

It is modelled on the reference command this tool replaces:

```text
ffmpeg -ss $Start -i "$File" -c:v $Vcodec -t $Duration -crf $CRF -y foo.mp4
```

`-ss` before `-i` (fast input seek) and `-t` after `-i` (unambiguous output-side
duration) are kept as-is. Three things are handled that the reference command does not:

- **Quality is codec-aware.** `-crf` is an x264/x265/SVT-AV1 parameter. NVENC ignores
  it and silently falls back to a default bitrate, so NVENC gets
  `-rc vbr -cq N -b:v 0` instead. A unit test asserts `-crf` never reaches an NVENC
  encoder.
- **Streams are mapped explicitly.** Default selection picks the "best" stream, which
  is a coin toss on multi-track recordings. `0:a:0?` keeps silent sources working.
- **Audio is explicit** (copy or AAC) rather than falling through to the container
  default.

## Architecture notes

### The render bridge (`src/render.rs`)

Slint hands out `GraphicsAPI::NativeOpenGL { get_proc_address }`; libmpv's render API
takes an `OpenGLInitParams { get_proc_address }`. These plug directly into each other.

- `RenderingSetup` — build the mpv `RenderContext` from Slint's GL loader
- `BeforeRendering` — mpv renders into our FBO-backed texture, which is wrapped with
  `BorrowedOpenGLTextureBuilder` and set as an ordinary Slint `image` property
- `RenderingTeardown` — drop the `RenderContext` while the GL context is still current

Three sharp edges, all handled in code and worth knowing about:

1. **`LC_NUMERIC` must be `"C"`** or `mpv_initialize` fails outright.
2. **Load the file only after the render context exists.** mpv initializes the video
   output at `loadfile` time; with no render context it fails and *permanently
   deselects the video track* (`Video: no video`). This is why `loadfile` is issued from
   inside `RenderingSetup` rather than before `run()`.
3. **mpv trashes GL state, and expects clean state itself.** `render_gl.h` says mpv
   "expect[s] that the OpenGL state is reasonably set to OpenGL standard defaults" and
   "will attempt to leave the OpenGL context with standard defaults", excluding viewport,
   scissor, `glBlendFuncSeparate` and `glClearColor`. We call it from inside Slint's
   frame, where femtovg has its own program, VAO and blend state bound — so `GlState`
   snapshots, resets to defaults, renders, then restores. Skipping the *reset* corrupts
   mpv's own output (visible as noise streaks across the video); skipping the *restore*
   corrupts the UI.
4. **Orientation is two independent inversions.** mpv's `flip` argument and the
   `BorrowedOpenGLTextureOrigin` each invert once, so they must agree: `flip=false` pairs
   with `TopLeft`. Getting this wrong is invisible on symmetric test patterns — verify
   with a video that has labelled corners.

Slint's default backend (winit + FemtoVG) is OpenGL-based on Windows, so no renderer
pinning is required. The GL context here reports as **OpenGL ES 3.2** via ANGLE; mpv
handles that fine and selects `nvdec` for hardware decode.

---

## Licensing

- **libmpv** — LGPL-2.1, dynamically linked (LGPL build, see above)
- **Slint** — royalty-free license requires attribution; an About box must carry
  "Built with Slint"
- **ffmpeg** — invoked as a subprocess, never linked
