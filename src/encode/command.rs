//! Builds ffmpeg argument vectors for clip exports.
//!
//! Pure: no I/O, no process spawning. Every export the app performs is
//! reproducible by pasting the produced argv into a terminal.
//!
//! Modelled on the reference command this tool was built to replace:
//!
//! ```text
//! ffmpeg -ss $Start -i "$File" -c:v $Vcodec -t $Duration -crf $CRF -y foo.mp4
//! ```
//!
//! `-ss` before `-i` performs a fast input seek. When re-encoding, ffmpeg still
//! decodes and discards up to the exact frame, so the cut is frame-accurate *and*
//! fast. `-t` after `-i` is an output option and is unambiguous across versions.

use std::path::{Path, PathBuf};

/// How the clip is produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CutMode {
    /// Stream copy. Instant and lossless, but the start snaps back to the
    /// nearest preceding keyframe.
    Fast,
    /// Re-encode. Honours the marks exactly, at the cost of encoding time.
    Precise,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum VideoEncoder {
    X264,
    NvencH264,
    NvencHevc,
    NvencAv1,
}

impl VideoEncoder {
    /// True when this encoder needs an NVIDIA GPU with NVENC.
    ///
    /// Presence in `ffmpeg -encoders` only proves the build has the encoder
    /// compiled in, not that a card is present — see [`crate::media::Tools`].
    pub fn needs_nvidia(self) -> bool {
        self.is_nvenc()
    }

    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            Self::X264 => "libx264",
            Self::NvencH264 => "h264_nvenc",
            Self::NvencHevc => "hevc_nvenc",
            Self::NvencAv1 => "av1_nvenc",
        }
    }

    fn is_nvenc(self) -> bool {
        !matches!(self, Self::X264)
    }

    /// Inclusive range of the quality scale, and a sane default.
    pub fn quality_range(self) -> (u32, u32, u32) {
        match self {
            // x264 CRF: 0 lossless, 23 default, 51 worst.
            Self::X264 => (0, 51, 20),
            // NVENC CQ uses the same nominal 0-51 scale.
            _ => (0, 51, 23),
        }
    }

    /// What the quality number is called in this encoder's own vocabulary.
    pub fn quality_label(self) -> &'static str {
        if self.is_nvenc() { "CQ" } else { "CRF" }
    }

    /// H.264 in MP4 is widely assumed to be 8-bit 4:2:0; forcing it avoids
    /// producing clips that some players refuse.
    fn wants_yuv420p(self) -> bool {
        matches!(self, Self::X264 | Self::NvencH264)
    }
}

/// Encoder-neutral speed/quality tradeoff, mapped per encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Speed {
    Fastest,
    Fast,
    Balanced,
    Slow,
    Slowest,
}

impl Speed {
    fn preset_for(self, enc: VideoEncoder) -> &'static str {
        if enc.is_nvenc() {
            // NVENC p1 (fastest) .. p7 (slowest).
            match self {
                Self::Fastest => "p1",
                Self::Fast => "p3",
                Self::Balanced => "p4",
                Self::Slow => "p6",
                Self::Slowest => "p7",
            }
        } else {
            match self {
                Self::Fastest => "ultrafast",
                Self::Fast => "veryfast",
                Self::Balanced => "medium",
                Self::Slow => "slow",
                Self::Slowest => "veryslow",
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AudioHandling {
    /// Pass the audio through untouched. Not always valid for the target
    /// container (e.g. AC3 into MP4), so the UI should offer AAC as a fallback.
    Copy,
    Aac {
        kbps: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Container {
    Mp4,
    Mkv,
    /// Audio only. The video track is dropped entirely.
    Mp3,
}

impl Container {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mkv => "mkv",
            Self::Mp3 => "mp3",
        }
    }

    /// True when the output carries no video, making every video setting moot.
    pub fn is_audio_only(self) -> bool {
        matches!(self, Self::Mp3)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct EncodeSettings {
    pub mode: CutMode,
    pub video: VideoEncoder,
    /// CRF for x264, CQ for NVENC. See [`VideoEncoder::quality_label`].
    pub quality: u32,
    pub speed: Speed,
    pub audio: AudioHandling,
    pub container: Container,
}

impl Default for EncodeSettings {
    fn default() -> Self {
        Self {
            mode: CutMode::Precise,
            video: VideoEncoder::X264,
            quality: 20,
            speed: Speed::Balanced,
            audio: AudioHandling::Aac { kbps: 192 },
            container: Container::Mp4,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExportRequest {
    pub input: PathBuf,
    pub output: PathBuf,
    /// In-mark, seconds from the start of the source.
    pub start: f64,
    /// Out-mark minus in-mark, in seconds.
    pub duration: f64,
    /// Source audio stream index, or `None` to let ffmpeg choose.
    pub audio_track: Option<usize>,
    pub settings: EncodeSettings,
}

/// Seconds as `HH:MM:SS.mmm` — see [`crate::timecode`] for why not a bare float.
pub use crate::timecode::ffmpeg_timestamp as format_timestamp;

/// Build the full argument vector, excluding the ffmpeg executable itself.
pub fn build_args(req: &ExportRequest) -> Vec<String> {
    let s = &req.settings;
    let mut a: Vec<String> = Vec::new();

    /// Appends each argument, stringified.
    macro_rules! push {
        ($($v:expr),+ $(,)?) => {{ $( a.push($v.to_string()); )+ }};
    }

    push!("-hide_banner");
    // Never block waiting on console input; we are not a terminal.
    push!("-nostdin");

    // Fast input seek. With re-encoding ffmpeg still lands on the exact frame.
    push!("-ss", format_timestamp(req.start));
    push!("-i", path_arg(&req.input));

    // Explicit mapping: default stream selection picks "best", which is a
    // coin toss on multi-track recordings.
    if !s.container.is_audio_only() {
        push!("-map", "0:v:0");
    }
    match req.audio_track {
        Some(i) => push!("-map", format!("0:a:{}", i)),
        // The trailing '?' tolerates sources with no audio at all.
        None => push!("-map", "0:a:0?"),
    }

    if s.container.is_audio_only() {
        // An audio-only container has nothing to copy the video into, so the
        // Fast/Precise distinction does not apply — there is only the audio.
        push!("-vn", "-c:a", "libmp3lame");
        match s.audio {
            // "Copy" cannot mean literal passthrough here; fall back to a good
            // default rather than producing an unplayable file.
            AudioHandling::Copy => push!("-q:a", "2"),
            AudioHandling::Aac { kbps } => push!("-b:a", format!("{}k", kbps)),
        }
        push!("-t", format_timestamp(req.duration));
        push!("-progress", "pipe:1", "-nostats");
        push!("-y", path_arg(&req.output));
        return a;
    }

    match s.mode {
        CutMode::Fast => {
            push!("-c", "copy");
            // Rebase timestamps so the clip starts at zero.
            push!("-avoid_negative_ts", "make_zero");
        }
        CutMode::Precise => {
            push!("-c:v", s.video.ffmpeg_name());

            if s.video.is_nvenc() {
                // NVENC ignores -crf. Constant-quality mode requires -rc vbr
                // with -b:v 0, otherwise -cq is overridden by a default bitrate.
                push!("-rc", "vbr", "-cq", s.quality, "-b:v", "0");
            } else {
                push!("-crf", s.quality);
            }

            push!("-preset", s.speed.preset_for(s.video));

            if s.video.wants_yuv420p() {
                push!("-pix_fmt", "yuv420p");
            }

            match s.audio {
                AudioHandling::Copy => push!("-c:a", "copy"),
                AudioHandling::Aac { kbps } => {
                    push!("-c:a", "aac", "-b:a", format!("{}k", kbps))
                }
            }
        }
    }

    // Output-side duration: unambiguous, unlike -to after a -ss input seek.
    push!("-t", format_timestamp(req.duration));

    if s.container == Container::Mp4 {
        // Move the index to the front so the clip is seekable immediately.
        push!("-movflags", "+faststart");
    }

    // Machine-readable progress on stdout; suppress the human stats on stderr.
    push!("-progress", "pipe:1", "-nostats");

    push!("-y", path_arg(&req.output));

    a
}

/// Render the command as a copy-pasteable shell line, for logs and error reports.
pub fn display_command(ffmpeg: &Path, args: &[String]) -> String {
    let quote = |s: &str| {
        if s.contains(' ') {
            format!("\"{s}\"")
        } else {
            s.to_string()
        }
    };
    let mut out = quote(&ffmpeg.display().to_string());
    for arg in args {
        out.push(' ');
        out.push_str(&quote(arg));
    }
    out
}

fn path_arg(p: &Path) -> String {
    p.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(settings: EncodeSettings) -> ExportRequest {
        ExportRequest {
            input: PathBuf::from("in.mkv"),
            output: PathBuf::from("out.mp4"),
            start: 62.5,
            duration: 10.0,
            audio_track: None,
            settings,
        }
    }

    /// Index of `flag`, or None. Used to assert argument ordering.
    fn pos(args: &[String], flag: &str) -> Option<usize> {
        args.iter().position(|a| a == flag)
    }

    fn value_of(args: &[String], flag: &str) -> Option<String> {
        pos(args, flag).and_then(|i| args.get(i + 1)).cloned()
    }

    #[test]
    fn seek_precedes_input_for_fast_seeking() {
        let args = build_args(&req(EncodeSettings::default()));
        let ss = pos(&args, "-ss").expect("-ss present");
        let i = pos(&args, "-i").expect("-i present");
        assert!(ss < i, "-ss must come before -i to get a fast input seek");
        assert_eq!(value_of(&args, "-ss").as_deref(), Some("00:01:02.500"));
    }

    #[test]
    fn duration_is_an_output_option() {
        let args = build_args(&req(EncodeSettings::default()));
        let i = pos(&args, "-i").unwrap();
        let t = pos(&args, "-t").unwrap();
        assert!(t > i, "-t must follow -i so it is an output-side duration");
        assert_eq!(value_of(&args, "-t").as_deref(), Some("00:00:10.000"));
    }

    #[test]
    fn x264_uses_crf() {
        let args = build_args(&req(EncodeSettings {
            video: VideoEncoder::X264,
            quality: 18,
            speed: Speed::Slow,
            ..Default::default()
        }));
        assert_eq!(value_of(&args, "-c:v").as_deref(), Some("libx264"));
        assert_eq!(value_of(&args, "-crf").as_deref(), Some("18"));
        assert_eq!(value_of(&args, "-preset").as_deref(), Some("slow"));
        assert!(pos(&args, "-cq").is_none(), "x264 must not receive -cq");
    }

    /// The bug the reference command would have hit: NVENC silently ignores
    /// -crf and falls back to a default bitrate.
    #[test]
    fn nvenc_uses_cq_and_never_crf() {
        for enc in [
            VideoEncoder::NvencH264,
            VideoEncoder::NvencHevc,
            VideoEncoder::NvencAv1,
        ] {
            let args = build_args(&req(EncodeSettings {
                video: enc,
                quality: 26,
                ..Default::default()
            }));
            assert!(
                pos(&args, "-crf").is_none(),
                "{enc:?} must not receive -crf: NVENC ignores it"
            );
            assert_eq!(value_of(&args, "-cq").as_deref(), Some("26"));
            // Without these, -cq is overridden by a default bitrate.
            assert_eq!(value_of(&args, "-rc").as_deref(), Some("vbr"));
            assert_eq!(value_of(&args, "-b:v").as_deref(), Some("0"));
        }
    }

    #[test]
    fn nvenc_presets_use_the_p_scale() {
        let args = build_args(&req(EncodeSettings {
            video: VideoEncoder::NvencH264,
            speed: Speed::Slowest,
            ..Default::default()
        }));
        assert_eq!(value_of(&args, "-preset").as_deref(), Some("p7"));
    }

    #[test]
    fn fast_mode_copies_streams_and_ignores_encoder_settings() {
        let args = build_args(&req(EncodeSettings {
            mode: CutMode::Fast,
            video: VideoEncoder::X264,
            quality: 12,
            ..Default::default()
        }));
        assert_eq!(value_of(&args, "-c").as_deref(), Some("copy"));
        assert_eq!(
            value_of(&args, "-avoid_negative_ts").as_deref(),
            Some("make_zero")
        );
        assert!(pos(&args, "-crf").is_none());
        assert!(pos(&args, "-c:v").is_none());
    }

    #[test]
    fn audio_handling_is_explicit() {
        let copy = build_args(&req(EncodeSettings {
            audio: AudioHandling::Copy,
            ..Default::default()
        }));
        assert_eq!(value_of(&copy, "-c:a").as_deref(), Some("copy"));

        let aac = build_args(&req(EncodeSettings {
            audio: AudioHandling::Aac { kbps: 256 },
            ..Default::default()
        }));
        assert_eq!(value_of(&aac, "-c:a").as_deref(), Some("aac"));
        assert_eq!(value_of(&aac, "-b:a").as_deref(), Some("256k"));
    }

    #[test]
    fn missing_audio_stream_is_tolerated() {
        let args = build_args(&req(EncodeSettings::default()));
        assert!(
            args.iter().any(|a| a == "0:a:0?"),
            "optional-stream '?' keeps silent sources from failing the export"
        );
    }

    #[test]
    fn explicit_audio_track_is_mapped() {
        let mut r = req(EncodeSettings::default());
        r.audio_track = Some(2);
        let args = build_args(&r);
        assert!(args.iter().any(|a| a == "0:a:2"));
    }

    #[test]
    fn mp3_drops_the_video_entirely() {
        let args = build_args(&req(EncodeSettings {
            container: Container::Mp3,
            ..Default::default()
        }));
        assert!(args.iter().any(|a| a == "-vn"), "video must be disabled");
        assert!(
            !args.iter().any(|a| a == "0:v:0"),
            "no video stream should be mapped into an audio-only container"
        );
        assert_eq!(value_of(&args, "-c:a").as_deref(), Some("libmp3lame"));
        assert!(pos(&args, "-c:v").is_none());
        assert!(pos(&args, "-movflags").is_none());
    }

    /// Stream copy is meaningless for MP3: there is no video to copy, and the
    /// audio must be re-encoded to MP3 regardless of the chosen mode.
    #[test]
    fn mp3_ignores_the_cut_mode() {
        let fast = build_args(&req(EncodeSettings {
            container: Container::Mp3,
            mode: CutMode::Fast,
            ..Default::default()
        }));
        assert!(
            !fast.iter().any(|a| a == "copy"),
            "Fast mode must not produce a stream copy into an MP3"
        );
        assert_eq!(value_of(&fast, "-c:a").as_deref(), Some("libmp3lame"));
    }

    #[test]
    fn mp3_falls_back_to_a_quality_target_when_audio_is_set_to_copy() {
        let args = build_args(&req(EncodeSettings {
            container: Container::Mp3,
            audio: AudioHandling::Copy,
            ..Default::default()
        }));
        // Passing -c:a copy here would yield an unplayable file.
        assert_eq!(value_of(&args, "-q:a").as_deref(), Some("2"));
        assert!(!args.iter().any(|a| a == "copy"));
    }

    #[test]
    fn mp3_still_honours_the_marks() {
        let args = build_args(&req(EncodeSettings {
            container: Container::Mp3,
            ..Default::default()
        }));
        assert_eq!(value_of(&args, "-ss").as_deref(), Some("00:01:02.500"));
        assert_eq!(value_of(&args, "-t").as_deref(), Some("00:00:10.000"));
        assert_eq!(value_of(&args, "-progress").as_deref(), Some("pipe:1"));
    }

    #[test]
    fn faststart_only_for_mp4() {
        let mp4 = build_args(&req(EncodeSettings {
            container: Container::Mp4,
            ..Default::default()
        }));
        assert!(pos(&mp4, "-movflags").is_some());

        let mkv = build_args(&req(EncodeSettings {
            container: Container::Mkv,
            ..Default::default()
        }));
        assert!(
            pos(&mkv, "-movflags").is_none(),
            "+faststart is an MP4 concept"
        );
    }

    #[test]
    fn progress_is_machine_readable() {
        let args = build_args(&req(EncodeSettings::default()));
        assert_eq!(value_of(&args, "-progress").as_deref(), Some("pipe:1"));
        assert!(pos(&args, "-nostats").is_some());
    }

    #[test]
    fn command_renders_with_quoted_paths() {
        let mut r = req(EncodeSettings::default());
        r.input = PathBuf::from("C:\\my videos\\a.mkv");
        let line = display_command(Path::new("ffmpeg"), &build_args(&r));
        assert!(line.contains("\"C:\\my videos\\a.mkv\""));
    }
}
