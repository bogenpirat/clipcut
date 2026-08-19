//! Prints the ffmpeg commands ClipCut would run, for eyeballing and debugging.
//!
//! ```text
//! cargo run --example show-commands
//! ```

use std::path::{Path, PathBuf};

use clipcut::encode::{
    AudioHandling, Container, CutMode, EncodeSettings, ExportRequest, Speed, VideoEncoder,
    build_args, display_command,
};

fn main() {
    let cases: [(&str, EncodeSettings); 4] = [
        (
            "Precise / libx264 (closest to the reference command)",
            EncodeSettings {
                mode: CutMode::Precise,
                video: VideoEncoder::X264,
                quality: 20,
                speed: Speed::Balanced,
                audio: AudioHandling::Aac { kbps: 192 },
                container: Container::Mp4,
            },
        ),
        (
            "Precise / NVENC H.264 - note -cq, not -crf",
            EncodeSettings {
                mode: CutMode::Precise,
                video: VideoEncoder::NvencH264,
                quality: 23,
                speed: Speed::Slow,
                audio: AudioHandling::Aac { kbps: 192 },
                container: Container::Mp4,
            },
        ),
        (
            "Precise / NVENC AV1, audio passed through",
            EncodeSettings {
                mode: CutMode::Precise,
                video: VideoEncoder::NvencAv1,
                quality: 28,
                speed: Speed::Balanced,
                audio: AudioHandling::Copy,
                container: Container::Mkv,
            },
        ),
        (
            "Fast / stream copy - marks snap to keyframes",
            EncodeSettings {
                mode: CutMode::Fast,
                container: Container::Mkv,
                ..Default::default()
            },
        ),
    ];

    for (label, settings) in cases {
        let ext = settings.container.extension();
        let req = ExportRequest {
            input: PathBuf::from(r"D:\recordings\Session 2026-08-14.mkv"),
            output: PathBuf::from(format!(r"D:\clips\Session 2026-08-14_001.{ext}")),
            start: 754.5,
            duration: 32.0,
            audio_track: None,
            settings,
        };
        println!("\n# {label}");
        println!(
            "{}",
            display_command(Path::new("ffmpeg"), &build_args(&req))
        );
    }
    println!();
}
