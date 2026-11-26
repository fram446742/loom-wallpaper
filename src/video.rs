//! Video wallpapers for the native backend.
//!
//! `ffmpeg` decodes the file into frames at the size of the output buffer; a
//! reader thread keeps the newest frame and the panel's frame timer commits it.
//! This is a CPU blit path by design: it needs no GPU decoder or Vulkan
//! surface, works on every compositor with layer-shell, and reuses the same
//! shm commit path as images.
//!
//! Two decode formats exist. The default is 8-bit BGRA, with HDR sources
//! tone-mapped to SDR through zscale/tonemap. When the niri output is currently
//! HDR and the compositor accepts a PQ/BT.2020 surface, HDR sources are instead
//! decoded to 10-bit packed PQ/BT.2020 (`x2rgb10le`), which `wl_shm` carries as
//! `Xrgb2101010`, and the compositor is told the exact colour encoding.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::geom::Size;

pub const FPS: u32 = 30;

/// `zscale` chain converting an HDR source to full-range PQ/BT.2020.
const HDR_TO_PQ: &str = "zscale=t=smpte2084:m=gbr:p=bt2020:r=full";
/// `zscale` chain tone-mapping an HDR source to limited-range BT.709 SDR.
const HDR_TO_SDR: &str = "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,\
                          tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p";

/// What the source video's transfer characteristic looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Sdr,
    Hdr,
}

/// Pixel layout the decoder produces, which the surface must declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeFormat {
    /// 8-bit BGRA, tone-mapped when the source is HDR.
    Sdr,
    /// 10-bit `x2rgb10le`, PQ transfer and BT.2020 primaries.
    Hdr,
}

/// Extensions handled by the video path.
pub fn is_video(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("mp4" | "mkv" | "webm" | "mov" | "m4v" | "avi")
    )
}

/// Chooses the decode format for one video on one output.
///
/// The HDR path needs all four conditions: an HDR source, an output the
/// compositor is currently signalling HDR on, a ready parametric PQ/BT.2020
/// image description, and a compositor that accepts 10-bit shm buffers. Any
/// missing condition keeps the SDR path, including SDR video on HDR outputs.
pub fn decode_format(
    source: Source,
    hdr_active: bool,
    color_management: bool,
    ten_bit_shm: bool,
) -> DecodeFormat {
    if source == Source::Hdr && hdr_active && color_management && ten_bit_shm {
        DecodeFormat::Hdr
    } else {
        DecodeFormat::Sdr
    }
}

pub struct Video {
    path: PathBuf,
    size: Size,
    format: DecodeFormat,
    child: Child,
    latest: Arc<Mutex<Arc<Vec<u8>>>>,
}

impl Video {
    /// Starts decoding `path` at `size`. Returns `None` when ffmpeg is missing
    /// or the process cannot be spawned; the caller paints the fallback colour.
    pub fn start(path: &Path, size: Size, format: DecodeFormat) -> Option<Self> {
        if size.is_empty() || !path.is_file() {
            return None;
        }
        let ffmpeg = std::env::var_os("LOOM_WALLPAPER_FFMPEG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("ffmpeg"));
        let filter = filter_chain(size, format, source(path));
        let pixel_format = match format {
            DecodeFormat::Sdr => "bgra",
            DecodeFormat::Hdr => "x2rgb10le",
        };
        let mut child = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-stream_loop",
                "-1",
                "-i",
            ])
            .arg(path)
            .args([
                "-vf",
                &filter,
                "-f",
                "rawvideo",
                "-pix_fmt",
                pixel_format,
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut stdout = child.stdout.take()?;
        let frame_len = size.width as usize * size.height as usize * 4;
        let latest = Arc::new(Mutex::new(Arc::new(Vec::new())));
        let shared = Arc::clone(&latest);
        std::thread::Builder::new()
            .name(format!(
                "wallpaper-video-{}",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("video")
            ))
            .spawn(move || {
                let mut buffer = vec![0u8; frame_len];
                while stdout.read_exact(&mut buffer).is_ok() {
                    if let Ok(mut latest) = shared.lock() {
                        *latest = Arc::new(buffer.clone());
                    }
                }
            })
            .ok()?;
        Some(Self {
            path: path.to_path_buf(),
            size,
            format,
            child,
            latest,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn size(&self) -> Size {
        self.size
    }

    pub fn format(&self) -> DecodeFormat {
        self.format
    }

    /// Newest decoded frame, or `None` until the first one arrives.
    pub fn frame(&self) -> Option<Arc<Vec<u8>>> {
        self.latest
            .lock()
            .ok()
            .map(|frame| Arc::clone(&frame))
            .filter(|frame| !frame.is_empty())
    }

    /// Whether the decoder is still running (used to restart it).
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `-vf` chain: optional transfer conversion, cover-scale/crop, fps, output
/// pixel format. HDR sources get either the PQ or the tone-map conversion
/// depending on the target format.
pub fn filter_chain(size: Size, format: DecodeFormat, source: Source) -> String {
    let mut filters = Vec::new();
    match (format, source) {
        (DecodeFormat::Hdr, _) => filters.push(HDR_TO_PQ.to_owned()),
        (DecodeFormat::Sdr, Source::Hdr) => filters.push(HDR_TO_SDR.to_owned()),
        (DecodeFormat::Sdr, Source::Sdr) => {}
    }
    filters.push(format!(
        "scale={}:{}:force_original_aspect_ratio=increase,crop={}:{}",
        size.width, size.height, size.width, size.height
    ));
    filters.push(format!("fps={FPS}"));
    filters.push(match format {
        DecodeFormat::Sdr => "format=bgra".to_owned(),
        DecodeFormat::Hdr => "format=x2rgb10le".to_owned(),
    });
    filters.join(",")
}

/// Transfer characteristic of the first video stream, from `ffprobe`.
pub fn source(path: &Path) -> Source {
    let ffprobe = std::env::var_os("LOOM_WALLPAPER_FFPROBE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("ffprobe"));
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=color_transfer",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else {
        return Source::Sdr;
    };
    let transfer = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    if transfer.contains("smpte2084")
        || transfer.contains("arib-std-b67")
        || transfer.contains("smpte428")
    {
        Source::Hdr
    } else {
        Source::Sdr
    }
}

/// Used by diagnostics to report the decode rate.
pub fn frame_interval() -> Duration {
    Duration::from_millis((1000 / FPS) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_video_extensions() {
        for name in ["a.mp4", "b.mkv", "c.WEBM", "d.mov"] {
            assert!(is_video(Path::new(name)), "{name}");
        }
        for name in ["a.png", "b.jpg", "c.svg", "noext"] {
            assert!(!is_video(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn filter_chain_covers_and_formats_bgra() {
        let chain = filter_chain(Size::new(1920, 1080), DecodeFormat::Sdr, Source::Sdr);
        assert!(
            chain.contains("scale=1920:1080:force_original_aspect_ratio=increase"),
            "{chain}"
        );
        assert!(chain.contains("crop=1920:1080"), "{chain}");
        assert!(chain.contains("fps=30"), "{chain}");
        assert!(chain.ends_with("format=bgra"), "{chain}");
        assert!(!chain.contains("tonemap"), "{chain}");
    }

    #[test]
    fn sdr_chain_tone_maps_hdr_sources() {
        let chain = filter_chain(Size::new(640, 480), DecodeFormat::Sdr, Source::Hdr);
        assert!(chain.contains("tonemap=hable"), "{chain}");
        assert!(chain.contains("format=yuv420p"), "{chain}");
        assert!(chain.ends_with("format=bgra"), "{chain}");
    }

    #[test]
    fn hdr_chain_is_pq_bt2020_10bit_without_tonemapping() {
        let chain = filter_chain(Size::new(3840, 2160), DecodeFormat::Hdr, Source::Hdr);
        assert!(chain.contains("zscale=t=smpte2084"), "{chain}");
        assert!(chain.contains("p=bt2020"), "{chain}");
        assert!(chain.contains("r=full"), "{chain}");
        assert!(chain.contains("scale=3840:2160"), "{chain}");
        assert!(chain.contains("m=gbr"), "{chain}");
        assert!(!chain.contains("in_color_matrix"), "{chain}");
        assert!(chain.ends_with("format=x2rgb10le"), "{chain}");
        assert!(!chain.contains("tonemap"), "{chain}");
    }

    #[test]
    fn hdr_needs_every_condition() {
        assert_eq!(
            decode_format(Source::Hdr, true, true, true),
            DecodeFormat::Hdr
        );
        for (hdr_active, color, ten_bit) in [
            (false, true, true),
            (true, false, true),
            (true, true, false),
        ] {
            assert_eq!(
                decode_format(Source::Hdr, hdr_active, color, ten_bit),
                DecodeFormat::Sdr,
                "hdr_active={hdr_active} color_management={color} ten_bit={ten_bit}"
            );
        }
        // SDR video stays 8-bit even on a fully capable HDR output.
        assert_eq!(
            decode_format(Source::Sdr, true, true, true),
            DecodeFormat::Sdr
        );
    }

    #[test]
    fn missing_protocol_or_description_keeps_the_sdr_path() {
        // The `color_management` flag means both the global and a ready
        // image description; either being absent lands here.
        assert_eq!(
            decode_format(Source::Hdr, true, false, true),
            DecodeFormat::Sdr
        );
    }

    #[test]
    fn decodes_frames_when_ffmpeg_is_present() {
        let dir = std::env::temp_dir().join(format!("loom-wallpaper-video-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("clip.mp4");
        let made = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=16x16:d=0.3",
                "-pix_fmt",
                "yuv420p",
                "-y",
            ])
            .arg(&clip)
            .status();
        if !made.is_ok_and(|status| status.success()) {
            return;
        }
        let mut video = Video::start(&clip, Size::new(8, 8), DecodeFormat::Sdr).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while video.frame().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            assert!(video.alive(), "decoder exited before the first frame");
        }
        let frame = video.frame().expect("no frame decoded");
        assert_eq!(frame.len(), 8 * 8 * 4);
        // Opaque red in BGRA (YUV conversion may round by one).
        assert_eq!(frame[3], 255);
        assert!(frame[2] > 250, "red={}", frame[2]);
        assert!(frame[0] < 5 && frame[1] < 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn frame_interval_matches_fps() {
        assert_eq!(frame_interval(), Duration::from_millis(33));
    }
}
