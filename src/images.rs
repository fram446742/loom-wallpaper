//! Image loading, modification detection and aspect-fill rendering to a
//! `wl_shm` buffer.
//!
//! The decode step is only used by the native backend; keeping it separate from
//! the Wayland code makes the reload rules testable without a compositor.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use image::{RgbaImage, imageops};

use crate::geom::{Size, cover};

/// File identity used to decide whether a decoded image is still current.
///
/// Modification time is the primary signal; the length is compared as well so
/// that a rewrite within the filesystem's mtime granularity (common on
/// one-second-resolution filesystems) is still detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStamp {
    pub modified: Option<SystemTime>,
    pub len: u64,
}

impl FileStamp {
    /// Reads the stamp of `path`, or `None` when the file cannot be stat'ed.
    pub fn from_path(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            modified: meta.modified().ok(),
            len: meta.len(),
        })
    }
}

/// Whether a cached image must be decoded again.
///
/// Two equal stamps mean the file is unchanged; two missing files mean there is
/// nothing to (re)load yet. Anything else is a change, including a file that
/// disappeared, so callers can drop stale entries.
pub fn needs_reload(previous: Option<&FileStamp>, current: Option<&FileStamp>) -> bool {
    match (previous, current) {
        (Some(previous), Some(current)) => previous != current,
        (None, None) => false,
        _ => true,
    }
}

/// A decoded image plus the stamp it was decoded from.
#[derive(Debug)]
struct Entry {
    pixels: RgbaImage,
    stamp: Option<FileStamp>,
}

/// Decoded images keyed by path, re-decoded only when the file changed.
#[derive(Debug, Default)]
pub struct ImageCache {
    entries: HashMap<PathBuf, Entry>,
}

impl ImageCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.entries.contains_key(path)
    }

    /// Whether the cached copy of `path` is older than the file on disk (or is
    /// absent, or the file disappeared).
    pub fn is_stale(&self, path: &Path) -> bool {
        let current = FileStamp::from_path(path);
        match self.entries.get(path) {
            Some(entry) => needs_reload(entry.stamp.as_ref(), current.as_ref()),
            None => true,
        }
    }

    pub fn forget(&mut self, path: &Path) {
        self.entries.remove(path);
    }

    /// Drops entries that are no longer referenced by `paths`.
    pub fn retain(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        let keep: std::collections::HashSet<PathBuf> = paths.into_iter().collect();
        self.entries.retain(|path, _| keep.contains(path));
    }

    /// Returns the decoded image for `path`, loading or reloading as needed.
    pub fn get(&mut self, path: &Path) -> Result<&RgbaImage, String> {
        if self.is_stale(path) {
            let loaded = load(path)?;
            self.entries.insert(path.to_path_buf(), loaded);
        }
        Ok(&self.entries[path].pixels)
    }
}

fn load(path: &Path) -> Result<Entry, String> {
    let stamp = FileStamp::from_path(path);
    let reader = image::ImageReader::open(path)
        .map_err(|err| format!("cannot open {}: {err}", path.display()))?
        .with_guessed_format()
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    let decoded = reader
        .decode()
        .map_err(|err| format!("cannot decode {}: {err}", path.display()))?;
    Ok(Entry {
        pixels: decoded.to_rgba8(),
        stamp,
    })
}

/// Renders `image` into an XRGB8888 buffer of `dest` pixels, scaled to cover
/// `dest` and centered (aspect-fill).
pub fn render_cover(image: &RgbaImage, dest: Size) -> Result<Vec<u8>, String> {
    let source = Size::new(image.width(), image.height());
    let plan = cover(source, dest).ok_or_else(|| "cannot cover an empty destination".to_owned())?;
    let cropped = imageops::crop_imm(
        image,
        plan.source.x,
        plan.source.y,
        plan.source.width,
        plan.source.height,
    )
    .to_image();
    let scaled = imageops::resize(
        &cropped,
        plan.dest.width,
        plan.dest.height,
        imageops::FilterType::Triangle,
    );
    Ok(to_xrgb(&scaled))
}

/// Fills a buffer with one opaque colour (used before an image is available).
pub fn render_solid(dest: Size, rgb: [u8; 3]) -> Vec<u8> {
    let mut buffer = vec![0u8; dest.width as usize * dest.height as usize * 4];
    for pixel in buffer.as_chunks_mut::<4>().0 {
        pixel[0] = rgb[2];
        pixel[1] = rgb[1];
        pixel[2] = rgb[0];
        pixel[3] = 0xFF;
    }
    buffer
}

/// Converts RGBA pixels to the little-endian XRGB8888 layout used by `wl_shm`.
fn to_xrgb(image: &RgbaImage) -> Vec<u8> {
    let mut out = vec![0u8; image.width() as usize * image.height() as usize * 4];
    for (pixel, out) in image.pixels().zip(out.as_chunks_mut::<4>().0) {
        let [r, g, b, _] = pixel.0;
        out[0] = b;
        out[1] = g;
        out[2] = r;
        out[3] = 0xFF;
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use image::Rgba;

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "loom-wallpaper-images-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_png(path: &Path, width: u32, height: u32, color: Rgba<u8>) {
        let image = RgbaImage::from_pixel(width, height, color);
        image.save(path).unwrap();
    }

    #[test]
    fn needs_reload_handles_missing_and_changed_stamps() {
        let now = SystemTime::now();
        let stamp = FileStamp {
            modified: Some(now),
            len: 10,
        };
        assert!(!needs_reload(Some(&stamp), Some(&stamp)));
        let later = FileStamp {
            modified: Some(now + Duration::from_secs(2)),
            len: 10,
        };
        assert!(needs_reload(Some(&stamp), Some(&later)));
        let resized = FileStamp {
            modified: Some(now),
            len: 11,
        };
        assert!(needs_reload(Some(&stamp), Some(&resized)));
        assert!(needs_reload(None, Some(&stamp)));
        assert!(needs_reload(Some(&stamp), None));
        // No file before and none now: nothing to reload (avoids retrying a
        // missing image every tick).
        assert!(!needs_reload(None, None));
    }

    #[test]
    fn cache_reloads_after_file_change() {
        let dir = temp_dir("reload");
        let path = dir.join("wall.png");
        write_png(&path, 4, 4, Rgba([255, 0, 0, 255]));

        let mut cache = ImageCache::new();
        let image = cache.get(&path).unwrap();
        assert_eq!((image.width(), image.height()), (4, 4));
        assert!(!cache.is_stale(&path));

        // Rewrite with different content and dimensions: the size change also
        // makes the stamp differ even on coarse-mtime filesystems.
        write_png(&path, 6, 6, Rgba([0, 0, 255, 255]));
        assert!(cache.is_stale(&path));
        let image = cache.get(&path).unwrap();
        assert_eq!((image.width(), image.height()), (6, 6));
        assert_eq!(image.get_pixel(0, 0).0, [0, 0, 255, 255]);
        assert!(!cache.is_stale(&path));

        // A deleted file is stale and reports the missing path.
        std::fs::remove_file(&path).unwrap();
        assert!(cache.is_stale(&path));
        assert!(cache.get(&path).is_err());
    }

    #[test]
    fn stamp_of_missing_file_is_none() {
        assert!(FileStamp::from_path(Path::new("/nonexistent/wall.png")).is_none());
    }

    #[test]
    fn render_cover_fills_the_whole_buffer_and_keeps_the_centre() {
        // 2x1 red/blue image onto 4x2: exact 2x upscale, no crop needed.
        let mut image = RgbaImage::new(2, 1);
        image.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        image.put_pixel(1, 0, Rgba([0, 0, 255, 255]));
        let buffer = render_cover(&image, Size::new(4, 2)).unwrap();
        assert_eq!(buffer.len(), 4 * 2 * 4);
        assert_eq!(&buffer[0..3], &[0, 0, 255]); // XRGB8888 is BGR in memory
        let last = (4 + 3) * 4;
        assert_eq!(&buffer[last..last + 3], &[255, 0, 0]);
        assert!(
            buffer
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| pixel[3] == 0xFF)
        );
    }

    #[test]
    fn render_solid_is_opaque_and_ordered_bgr() {
        let buffer = render_solid(Size::new(2, 1), [0x11, 0x22, 0x33]);
        assert_eq!(buffer, vec![0x33, 0x22, 0x11, 0xFF, 0x33, 0x22, 0x11, 0xFF]);
    }
}
