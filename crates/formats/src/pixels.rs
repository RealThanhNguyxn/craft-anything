//! Turning game textures' stored pixels into RGBA: delegates decoding to [`craft_dds::pixels`]
//! while keeping per-engine layout lookup in `unity_texture` and `unreal_texture`.

use crate::common::FormatError;

pub use craft_dds::pixels::{DEFAULT_MAX_RGBA_BYTES, PixelLayout as Layout, flip_rows, size};

fn map_error(error: craft_dds::DdsError, width: u32, height: u32) -> FormatError {
    match error {
        craft_dds::DdsError::Truncated(msg) => FormatError::Truncated(msg),
        craft_dds::DdsError::Alloc(_) | craft_dds::DdsError::LimitExceeded { .. } => FormatError::TooLarge(format!("a {width}×{height} RGBA picture")),
        craft_dds::DdsError::Malformed(msg) | craft_dds::DdsError::Encode(msg) => FormatError::Malformed(msg),
        craft_dds::DdsError::Unsupported(msg) => FormatError::Unsupported(msg),
        _ => FormatError::Malformed("pixel decode failed".into()),
    }
}

/// Decode a `width`×`height` picture (`data` starts with it) into RGBA, rows in stored order.
///
/// Caps the output at [`DEFAULT_MAX_RGBA_BYTES`]; use [`decode_with_limit`] for a different budget.
pub fn decode(layout: Layout, width: u32, height: u32, data: &[u8]) -> Result<Vec<u8>, FormatError> {
    decode_with_limit(layout, width, height, data, DEFAULT_MAX_RGBA_BYTES)
}

/// Decode into RGBA, allowing at most `max_bytes` of output.
///
/// The budget applies to RGBA bytes, not the encoded input, and is checked before reading pixels
/// or reserving the output. Output reservation failures are reported as [`FormatError::TooLarge`].
pub fn decode_with_limit(layout: Layout, width: u32, height: u32, data: &[u8], max_bytes: u64) -> Result<Vec<u8>, FormatError> {
    craft_dds::pixels::decode_with_limit(layout, width, height, data, max_bytes).map_err(|err| map_error(err, width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncompressed_layouts() {
        assert_eq!(decode(Layout::Rgba32, 1, 2, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap(), [1, 2, 3, 4, 5, 6, 7, 8], "stored order");
        assert_eq!(decode(Layout::Argb32, 1, 1, &[9, 1, 2, 3]).unwrap(), [1, 2, 3, 9]);
        assert_eq!(decode(Layout::Bgra32, 1, 1, &[3, 2, 1, 9]).unwrap(), [1, 2, 3, 9]);
        assert_eq!(decode(Layout::Rgb24, 1, 1, &[1, 2, 3]).unwrap(), [1, 2, 3, 255]);
        assert_eq!(decode(Layout::Alpha8, 1, 1, &[7]).unwrap(), [255, 255, 255, 7]);
        assert_eq!(decode(Layout::R8, 1, 1, &[7]).unwrap(), [7, 7, 7, 255]);
        assert_eq!(decode(Layout::Rgb565, 1, 1, &0xf800u16.to_le_bytes()).unwrap(), [255, 0, 0, 255]);
        assert_eq!(decode(Layout::Rgba4444, 1, 1, &0xf00fu16.to_le_bytes()).unwrap(), [255, 0, 0, 255]);
        assert_eq!(decode(Layout::Argb4444, 1, 1, &0xff00u16.to_le_bytes()).unwrap(), [255, 0, 0, 255]);
        assert_eq!(flip_rows(&[1, 1, 1, 1, 2, 2, 2, 2], 1), [2, 2, 2, 2, 1, 1, 1, 1]);
        assert!(matches!(decode(Layout::Rgba32, 2, 2, &[0; 15]), Err(FormatError::Truncated(_))));
        assert!(decode(Layout::Rgba32, 0, 1, &[]).is_err());
    }

    #[test]
    fn block_layouts() {
        // DXT1: colour 0 red, colour 1 blue; the first texel uses index 1, the rest index 0.
        let px = decode(Layout::Dxt1, 4, 4, &[0x00, 0xf8, 0x1f, 0x00, 0b01, 0, 0, 0]).unwrap();
        assert_eq!((&px[..4], &px[4..8]), (&[0, 0, 255, 255][..], &[255, 0, 0, 255][..]));
        // DXT5: alpha 255 → 0; the first texel index 1 (alpha 0), the rest index 0 (255).
        let mut b = vec![255, 0, 1, 0, 0, 0, 0, 0];
        b.extend_from_slice(&[0x00, 0xf8, 0x1f, 0x00, 0, 0, 0, 0]);
        let px = decode(Layout::Dxt5, 4, 4, &b).unwrap();
        assert_eq!((px[3], px[7]), (0, 255));
        // BC4: grey from one channel block; BC5: red and green, blue rebuilt (flat normal → 255).
        assert_eq!(&decode(Layout::Bc4, 4, 4, &[200, 10, 0, 0, 0, 0, 0, 0]).unwrap()[..4], &[200, 200, 200, 255]);
        let mut n = vec![128, 128, 0, 0, 0, 0, 0, 0];
        n.extend_from_slice(&[128, 128, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&decode(Layout::Bc5, 4, 4, &n).unwrap()[..4], &[128, 128, 254, 255]);
        // 5×5 needs 4 blocks and is cropped.
        assert_eq!(decode(Layout::Dxt1, 5, 5, &[0; 32]).unwrap().len(), 5 * 5 * 4);
    }

    #[test]
    fn bptc_eac_and_packed_layout_parity() {
        for layout in [
            Layout::Dxt3,
            Layout::Bc6h,
            Layout::Bc6hSigned,
            Layout::Bc7,
            Layout::Etc1,
            Layout::EacR,
            Layout::EacRSigned,
            Layout::EacRg,
            Layout::EacRgSigned,
            Layout::Rgb9e5,
            Layout::Rgba1010102,
            Layout::Rgb111110Float,
        ] {
            let block_bytes = size(layout, 4, 4).unwrap();
            assert_eq!(Some(block_bytes), craft_dds::pixels::size(layout, 4, 4));
            let block_input = vec![0u8; block_bytes];
            assert_eq!(
                decode(layout, 4, 4, &block_input).unwrap(),
                craft_dds::pixels::decode_with_limit(layout, 4, 4, &block_input, DEFAULT_MAX_RGBA_BYTES).unwrap()
            );

            let partial_bytes = size(layout, 5, 3).unwrap();
            let partial_input = vec![0u8; partial_bytes];
            assert_eq!(decode(layout, 5, 3, &partial_input).unwrap().len(), 5 * 3 * 4);
            assert!(matches!(decode(layout, 4, 4, &block_input[..block_bytes - 1]), Err(FormatError::Truncated(_))));
            assert!(matches!(decode_with_limit(layout, 4, 4, &block_input, 4 * 4 * 4 - 1), Err(FormatError::TooLarge(_))));
        }
    }
}
