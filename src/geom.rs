//! Pure geometry for background surfaces: aspect-fill ("cover") crops, output
//! rotation and scale.
//!
//! Nothing here talks to Wayland or the filesystem, so every rule below is unit
//! testable. A layer-shell background surface is configured by the compositor
//! with its size in *surface-local* (logical) coordinates, which the compositor
//! already rotates for the output transform. To render at the physical
//! resolution the client multiplies that size by the output scale and asks the
//! compositor to scale the buffer (see [`Size::scaled`]); the image content is
//! never pre-rotated at runtime, [`Rotation`] exists for clients that choose to
//! pre-rotate a buffer instead.

/// A width/height pair in pixels (or logical units).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Size in buffer pixels for an output at `scale` (scale 2 doubles it).
    pub const fn scaled(self, scale: u32) -> Self {
        Self {
            width: self.width.saturating_mul(scale),
            height: self.height.saturating_mul(scale),
        }
    }

    /// Size after applying an output rotation. Quarter turns swap the axes.
    pub const fn rotated(self, rotation: Rotation) -> Self {
        if rotation.swaps_axes() {
            Self::new(self.height, self.width)
        } else {
            self
        }
    }

    /// Size in logical units for an output at `scale` (the inverse of
    /// [`Size::scaled`]).
    pub const fn divided(self, scale: u32) -> Self {
        let scale = if scale == 0 { 1 } else { scale };
        Self {
            width: self.width / scale,
            height: self.height / scale,
        }
    }
}

/// A quarter-turn output rotation. Flips are not represented: the compositor
/// applies them for us, and a mirrored wallpaper has no visible effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    #[default]
    Normal,
    Cw90,
    Cw180,
    Cw270,
}

impl Rotation {
    /// Builds a rotation from the number of clockwise quarter turns.
    pub const fn from_quarter_turns(turns: u8) -> Self {
        match turns % 4 {
            1 => Self::Cw90,
            2 => Self::Cw180,
            3 => Self::Cw270,
            _ => Self::Normal,
        }
    }

    pub const fn quarter_turns(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::Cw90 => 1,
            Self::Cw180 => 2,
            Self::Cw270 => 3,
        }
    }

    /// Whether applying this rotation swaps width and height.
    pub const fn swaps_axes(self) -> bool {
        matches!(self, Self::Cw90 | Self::Cw270)
    }
}

/// An integer pixel rectangle, used for the region of the source image that is
/// visible after an aspect-fill scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// The result of an aspect-fill scale: the source region to sample (centered)
/// and the destination size in buffer pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cover {
    pub source: Rect,
    pub dest: Size,
}

/// Computes a centered aspect-fill ("cover") scaling: the image is scaled up (or
/// down) until it covers `dest`, then the overflow is cropped equally on both
/// sides, so the center of the image stays centered.
///
/// Returns `None` when either size is zero.
pub fn cover(image: Size, dest: Size) -> Option<Cover> {
    if image.is_empty() || dest.is_empty() {
        return None;
    }
    let scale = f64::max(
        dest.width as f64 / image.width as f64,
        dest.height as f64 / image.height as f64,
    );
    // The visible part of the source: `dest / scale`, clamped to the image.
    let crop_width = ((dest.width as f64 / scale).round() as u32).clamp(1, image.width);
    let crop_height = ((dest.height as f64 / scale).round() as u32).clamp(1, image.height);
    Some(Cover {
        source: Rect {
            x: (image.width - crop_width) / 2,
            y: (image.height - crop_height) / 2,
            width: crop_width,
            height: crop_height,
        },
        dest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_crops_wide_images_centered() {
        // 16:9 image onto a 4:3 surface: full height, horizontal crop centered.
        let plan = cover(Size::new(1920, 1080), Size::new(1024, 768)).unwrap();
        assert_eq!(plan.dest, Size::new(1024, 768));
        assert_eq!(plan.source.width, 1440);
        assert_eq!(plan.source.height, 1080);
        assert_eq!(plan.source.x, 240);
        assert_eq!(plan.source.y, 0);
    }

    #[test]
    fn cover_crops_tall_images_centered() {
        let plan = cover(Size::new(1080, 2400), Size::new(1920, 1080)).unwrap();
        // scale = max(1920/1080, 1080/2400) = 1.777.. -> visible height 607.
        assert_eq!(plan.source.height, 608);
        assert_eq!(plan.source.x, 0);
        assert_eq!(plan.source.y, (2400 - 608) / 2);
        assert_eq!(plan.source.width, 1080);
    }

    #[test]
    fn cover_is_identity_when_aspect_matches() {
        let plan = cover(Size::new(1920, 1080), Size::new(3840, 2160)).unwrap();
        assert_eq!(
            plan.source,
            Rect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            }
        );
        assert_eq!(plan.dest, Size::new(3840, 2160));
    }

    #[test]
    fn cover_rejects_empty_sizes() {
        assert!(cover(Size::new(0, 10), Size::new(10, 10)).is_none());
        assert!(cover(Size::new(10, 10), Size::new(10, 0)).is_none());
    }

    #[test]
    fn scale_multiplies_buffer_pixels() {
        assert_eq!(Size::new(1920, 1080).scaled(2), Size::new(3840, 2160));
        assert_eq!(Size::new(1920, 1080).scaled(1), Size::new(1920, 1080));
        // Saturating instead of overflowing for absurd scales.
        assert_eq!(Size::new(u32::MAX, 2).scaled(2), Size::new(u32::MAX, 4));
        // Dividing by the scale returns logical units again.
        assert_eq!(Size::new(3840, 2160).divided(2), Size::new(1920, 1080));
        assert_eq!(Size::new(3840, 2160).divided(0), Size::new(3840, 2160));
    }

    #[test]
    fn rotation_swaps_axes_only_for_quarter_turns() {
        let size = Size::new(1920, 1080);
        assert_eq!(size.rotated(Rotation::Normal), size);
        assert_eq!(size.rotated(Rotation::Cw90), Size::new(1080, 1920));
        assert_eq!(size.rotated(Rotation::Cw180), size);
        assert_eq!(size.rotated(Rotation::Cw270), Size::new(1080, 1920));
        assert_eq!(Rotation::from_quarter_turns(5), Rotation::Cw90);
        assert_eq!(Rotation::from_quarter_turns(3).quarter_turns(), 3);
    }

    #[test]
    fn rotation_and_scale_compose() {
        // A rotated 1080x1920 output at scale 2 is 2160x3840 buffer pixels, and
        // its cover plan swaps the crop axes relative to the unrotated one.
        let logical = Size::new(1920, 1080);
        let buffer = logical.scaled(2).rotated(Rotation::Cw90);
        assert_eq!(buffer, Size::new(2160, 3840));
        let portrait = Size::new(1080, 1920);
        let rotated_plan = cover(portrait, buffer).unwrap();
        assert_eq!(rotated_plan.source.width, 1080);
        assert_eq!(rotated_plan.source.height, 1920);
        assert_eq!(rotated_plan.dest, buffer);
        let landscape_plan = cover(portrait, logical).unwrap();
        assert_eq!(landscape_plan.source.width, 1080);
        assert_eq!(landscape_plan.source.height, 608);
        assert_eq!(landscape_plan.source.y, (1920 - 608) / 2);
    }
}
