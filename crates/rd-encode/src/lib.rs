//! rd-encode — 编码器抽象、H.264（OpenH264 / ffmpeg 硬编）、脏区跟踪与 QOI+zstd 无损瓦片。

pub mod caps;
pub mod dirty;
pub mod frame;
pub mod h264_ffmpeg;
pub mod h264_openh264;
pub mod rate;
pub mod tiles;
