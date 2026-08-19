//! Clip export: building ffmpeg invocations and tracking their progress.

pub mod command;
pub mod progress;

// Consumed by the encoding panel and export job in later phases.
#[allow(unused_imports)]
pub use command::{
    AudioHandling, Container, CutMode, EncodeSettings, ExportRequest, Speed, VideoEncoder,
    build_args, display_command, format_timestamp,
};
#[allow(unused_imports)]
pub use progress::{Progress, ProgressParser};
