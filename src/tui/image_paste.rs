//! Encoding of raw clipboard images into a bounded, base64-encoded PNG that
//! can ride along on a user message as an image content part.
//!
//! The system clipboard hands us raw RGBA8 (see [`ClipboardImage`]); models
//! want an encoded, size-bounded image. We PNG-encode with fast compression
//! and, if the result exceeds the cap, box-filter downscale and retry a couple
//! of times before rejecting — the same 5 MiB ceiling the read tool enforces
//! on image files (`src/tool/read.rs`).

use anyhow::{Context, Result, bail};
use base64::Engine;

use crate::copy::ClipboardImage;

/// Encoded-PNG byte ceiling before base64. Mirrors `MAX_IMAGE_BYTES` in
/// `src/tool/read.rs` so pasted and file-read images share one limit.
pub(crate) const MAX_ENCODED_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// MIME type of every image this module emits — it always re-encodes to PNG.
pub(crate) const ENCODED_IMAGE_MIME: &str = "image/png";

/// Number of 2× downscale retries attempted before giving up on an oversize
/// image (so at most `1 + DOWNSCALE_ATTEMPTS` encode passes).
const DOWNSCALE_ATTEMPTS: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EncodedImage {
    /// Base64 (STANDARD) of the PNG bytes.
    pub base64: String,
    /// Length of the PNG bytes prior to base64 encoding.
    pub byte_len: usize,
    pub width: u32,
    pub height: u32,
}

/// Encode a raw RGBA8 clipboard image to a base64 PNG no larger than
/// `max_bytes`, downscaling by half up to [`DOWNSCALE_ATTEMPTS`] times when the
/// first encode overflows. Returns an error if the smallest attempt still
/// exceeds the cap or the pixel buffer is malformed.
pub(crate) fn encode_clipboard_image(
    image: &ClipboardImage,
    max_bytes: usize,
) -> Result<EncodedImage> {
    if image.width == 0 || image.height == 0 {
        bail!("Pasted image has zero dimensions.");
    }
    let expected = image
        .width
        .checked_mul(image.height)
        .and_then(|pixels| pixels.checked_mul(4));
    if expected != Some(image.rgba.len()) {
        bail!(
            "Pasted image buffer is malformed ({}x{}, {} bytes).",
            image.width,
            image.height,
            image.rgba.len()
        );
    }

    let mut width = image.width;
    let mut height = image.height;
    let mut rgba = image.rgba.clone();

    for attempt in 0..=DOWNSCALE_ATTEMPTS {
        let png = encode_png(width, height, &rgba)?;
        if png.len() <= max_bytes {
            return Ok(EncodedImage {
                base64: base64::engine::general_purpose::STANDARD.encode(&png),
                byte_len: png.len(),
                width: width as u32,
                height: height as u32,
            });
        }
        // Can't shrink a 1px axis any further — stop retrying.
        if attempt == DOWNSCALE_ATTEMPTS || (width <= 1 && height <= 1) {
            break;
        }
        let downscaled = downscale_half(width, height, &rgba);
        width = downscaled.0;
        height = downscaled.1;
        rgba = downscaled.2;
    }

    bail!(
        "Pasted image is too large to attach (over {} KiB even after downscaling).",
        max_bytes / 1024
    )
}

fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buf, width as u32, height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder
            .write_header()
            .context("Failed to write PNG header for pasted image")?;
        writer
            .write_image_data(rgba)
            .context("Failed to encode pasted image to PNG")?;
    }
    Ok(buf)
}

/// Average each 2x2 block into one output pixel. Odd dimensions leave a
/// partial edge block, which is averaged over however many source pixels it
/// covers, so no bytes are read out of bounds.
fn downscale_half(width: usize, height: usize, rgba: &[u8]) -> (usize, usize, Vec<u8>) {
    let new_width = (width / 2).max(1);
    let new_height = (height / 2).max(1);
    let mut out = vec![0u8; new_width * new_height * 4];

    for y in 0..new_height {
        for x in 0..new_width {
            for channel in 0..4 {
                let mut sum = 0u32;
                let mut count = 0u32;
                for dy in 0..2 {
                    for dx in 0..2 {
                        let src_x = x * 2 + dx;
                        let src_y = y * 2 + dy;
                        if src_x < width && src_y < height {
                            sum += u32::from(rgba[(src_y * width + src_x) * 4 + channel]);
                            count += 1;
                        }
                    }
                }
                out[(y * new_width + x) * 4 + channel] = (sum / count.max(1)) as u8;
            }
        }
    }

    (new_width, new_height, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_image(width: usize, height: usize, color: [u8; 4]) -> ClipboardImage {
        ClipboardImage {
            width,
            height,
            rgba: color
                .iter()
                .copied()
                .cycle()
                .take(width * height * 4)
                .collect(),
        }
    }

    #[test]
    fn encodes_small_image_to_png_base64() {
        let image = solid_image(4, 4, [10, 20, 30, 255]);
        let encoded = encode_clipboard_image(&image, MAX_ENCODED_IMAGE_BYTES).unwrap();
        assert_eq!(encoded.width, 4);
        assert_eq!(encoded.height, 4);

        let png = base64::engine::general_purpose::STANDARD
            .decode(&encoded.base64)
            .unwrap();
        assert_eq!(png.len(), encoded.byte_len);
        // PNG magic number.
        assert_eq!(
            &png[..8],
            &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']
        );
    }

    #[test]
    fn downscales_when_over_cap_then_succeeds() {
        // A large noisy image won't fit a tiny cap at full size but will once
        // downscaled. Vary bytes so compression can't collapse it to nothing.
        let width = 64;
        let height = 64;
        let rgba: Vec<u8> = (0..width * height * 4)
            .map(|i| (i * 7 % 251) as u8)
            .collect();
        let image = ClipboardImage {
            width,
            height,
            rgba,
        };

        let encoded = encode_clipboard_image(&image, 2048).unwrap();
        assert!(encoded.byte_len <= 2048);
        // Downscaled at least once from the 64x64 original.
        assert!(encoded.width < 64 && encoded.width >= 16);
    }

    #[test]
    fn rejects_image_that_stays_over_cap() {
        let width = 64;
        let height = 64;
        let rgba: Vec<u8> = (0..width * height * 4)
            .map(|i| (i * 13 % 251) as u8)
            .collect();
        let image = ClipboardImage {
            width,
            height,
            rgba,
        };

        // A cap of 8 bytes is smaller than any valid PNG, even a 16x16 one.
        let err = encode_clipboard_image(&image, 8).unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn rejects_malformed_buffer() {
        let image = ClipboardImage {
            width: 4,
            height: 4,
            rgba: vec![0; 10],
        };
        let err = encode_clipboard_image(&image, MAX_ENCODED_IMAGE_BYTES).unwrap_err();
        assert!(err.to_string().contains("malformed"));
    }
}
