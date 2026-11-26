//! PQ/BT.2020 helpers for the HDR video path.
//!
//! ffmpeg hands the native backend PQ-encoded BT.2020 frames directly, so the
//! only pixels this module encodes are the solid colour shown before the first
//! decoded frame arrives. The luminance constants match the parametric image
//! description attached to those surfaces: BT.2408 reference white on the PQ
//! transfer characteristic.

use crate::geom::Size;

/// Reference white luminance in cd/m² (Report ITU-R BT.2408).
pub const REFERENCE_WHITE: u32 = 203;
/// Primary colour volume minimum in 0.0001 cd/m² (0.005 cd/m², the PQ floor).
pub const MIN_LUMINANCE: u32 = 50;
/// Primary colour volume maximum in cd/m². PQ implies `min + 10 000`.
pub const MAX_LUMINANCE: u32 = 10_000;

/// Encodes an absolute luminance as a 10-bit PQ code.
pub fn pq_code(luminance: f64) -> u16 {
    const M1: f64 = 2610.0 / 16384.0;
    const M2: f64 = 2523.0 / 4096.0 * 128.0;
    const C1: f64 = 3424.0 / 4096.0;
    const C2: f64 = 2413.0 / 4096.0 * 32.0;
    const C3: f64 = 2392.0 / 4096.0 * 32.0;
    let normalized = (luminance / 10_000.0).clamp(0.0, 1.0);
    let power = normalized.powf(M1);
    let encoded = ((C1 + C2 * power) / (1.0 + C3 * power)).powf(M2);
    (encoded * 1023.0).round().clamp(0.0, 1023.0) as u16
}

/// Encodes an sRGB colour as BT.2020 PQ channel codes, treating white as the
/// BT.2408 reference white (203 cd/m²).
pub fn srgb_to_pq(rgb: [u8; 3]) -> [u16; 3] {
    let [r, g, b] = rgb.map(srgb_linear);
    // Linear sRGB (BT.709 primaries) to BT.2020, D65 white point.
    let linear = [
        0.627_404 * r + 0.329_282 * g + 0.043_313_6 * b,
        0.069_097 * r + 0.919_540 * g + 0.011_361_2 * b,
        0.016_391_6 * r + 0.088_013_2 * g + 0.895_595 * b,
    ];
    linear.map(|value| pq_code(value * f64::from(REFERENCE_WHITE)))
}

fn srgb_linear(value: u8) -> f64 {
    let value = f64::from(value) / 255.0;
    if value <= 0.040_45 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// Packs a 10-bit RGB triple into an `Xrgb2101010` word (`x:R:G:B` 2:10:10:10,
/// leaving the two pad bits zero like the DRM format).
pub const fn pack(rgb: [u16; 3]) -> u32 {
    ((rgb[0] as u32 & 0x3ff) << 20) | ((rgb[1] as u32 & 0x3ff) << 10) | (rgb[2] as u32 & 0x3ff)
}

/// A solid PQ/BT.2020 `Xrgb2101010` buffer, little-endian like `wl_shm`.
pub fn render_solid(size: Size, rgb: [u8; 3]) -> Vec<u8> {
    let pixel = pack(srgb_to_pq(rgb)).to_le_bytes();
    let mut buffer = Vec::with_capacity(size.width as usize * size.height as usize * 4);
    for _ in 0..size.width as usize * size.height as usize {
        buffer.extend_from_slice(&pixel);
    }
    buffer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn black_encodes_to_zero() {
        assert_eq!(srgb_to_pq([0, 0, 0]), [0, 0, 0]);
        assert_eq!(pq_code(0.0), 0);
    }

    #[test]
    fn sdr_white_is_the_203_nit_reference() {
        // 203 cd/m² on the PQ curve is ~0.5807, i.e. code 594.
        assert_eq!(srgb_to_pq([255, 255, 255]), [594, 594, 594]);
    }

    #[test]
    fn mid_tone_follows_the_srgb_eotf() {
        // 128/255 is 0.2159 linear, 43.8 cd/m², code 438.
        assert_eq!(srgb_to_pq([128, 128, 128]), [438, 438, 438]);
    }

    #[test]
    fn packing_matches_the_drm_layout() {
        assert_eq!(pack([1023, 0, 0]), 0x3ff0_0000);
        assert_eq!(pack([0, 1023, 0]), 0x000f_fc00);
        assert_eq!(pack([0, 0, 1023]), 0x0000_03ff);
        assert_eq!(pack([0, 0, 0]), 0);
        // Little-endian bytes: B is lowest, R highest, pad bits stay zero.
        assert_eq!(pack([1023, 0, 0]).to_le_bytes(), [0x00, 0x00, 0xf0, 0x3f]);
    }

    #[test]
    fn solid_buffer_repeats_the_encoded_pixel() {
        let buffer = render_solid(Size::new(2, 1), [0, 0, 0]);
        assert_eq!(buffer.len(), 2 * 4);
        assert_eq!(&buffer[..4], &[0, 0, 0, 0]);
        assert_eq!(&buffer[4..], &[0, 0, 0, 0]);
    }
}
