//! Presentation-side image helpers (AD-0010, AD-0011). The attachment
//! ladder owns what gets *sent*; these functions answer what can be
//! *shown* or *staged*: a header-only dimension sniff, a re-encode to
//! PNG — the one compressed format the kitty graphics protocol accepts
//! (`f=100`) — and raw RGBA pixels to PNG for clipboard pastes.

use crate::tools::image_resize::MAX_DECODE_DIMENSION;
use image::ImageEncoder;
use image::codecs::png::PngEncoder;
use image::{DynamicImage, ImageReader, Limits};
use std::borrow::Cow;
use std::io::Cursor;

/// Pixel dimensions of an encoded image, read from its headers — no
/// full decode. `None` when the bytes do not parse as a known image.
pub fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// The bytes as PNG, ready for a kitty `f=100` transmission. PNG input
/// is passed through untouched (borrowed); anything else the decoder
/// accepts is re-encoded. `None` on corrupt input — the caller falls
/// back to its text note.
/// The bytes as PNG, ready for a kitty `f=100` transmission. PNG input
/// is passed through untouched (borrowed) — but only after a structural
/// chunk-walk, because a corrupt "PNG" transmitted blind would render
/// as nothing instead of falling back to the honest text note.
/// Anything else the decoder accepts is re-encoded. `None` on corrupt
/// input.
pub fn to_png(bytes: &[u8]) -> Option<Cow<'_, [u8]>> {
    if valid_png(bytes) {
        return Some(Cow::Borrowed(bytes));
    }
    let mut limits = Limits::default(); // #[non_exhaustive]: built by assignment
    limits.max_image_width = Some(MAX_DECODE_DIMENSION);
    limits.max_image_height = Some(MAX_DECODE_DIMENSION);
    let mut reader = ImageReader::new(Cursor::new(bytes));
    reader.limits(limits);
    let decoded: DynamicImage = reader.with_guessed_format().ok()?.decode().ok()?;
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(
            decoded.as_bytes(),
            decoded.width(),
            decoded.height(),
            decoded.color().into(),
        )
        .ok()?;
    Some(Cow::Owned(png))
}

/// Magic bytes plus a chunk walk to IEND — length fields only, no CRC
/// verify. The same "never trust the extension, only the bytes" rule
/// `read` uses, applied one level deeper: never trust the magic alone,
/// only the structure.
/// Encode raw RGBA8 pixels as PNG — what the system clipboard hands
/// over on a paste (arboard decodes to pixels; it never hands back an
/// encoded format). `None` on shape mismatch or encode failure; the
/// paste then degrades to its note instead of a staged path.
pub fn from_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    if width == 0 || height == 0 {
        return None;
    }
    // u64 math: a hostile w*h from a broken clipboard must not
    // overflow the usize product check.
    let expected = width as u64 * height as u64 * 4;
    if expected != rgba.len() as u64 {
        return None;
    }
    let buffer = image::RgbaImage::from_raw(width, height, rgba.to_vec())?;
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(
            buffer.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .ok()?;
    Some(out)
}

fn valid_png(bytes: &[u8]) -> bool {
    if !bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return false;
    }
    let mut rest = &bytes[8..];
    while rest.len() >= 8 {
        let len = u32::from_be_bytes(rest[0..4].try_into().unwrap()) as usize;
        let Some(end) = 8usize.checked_add(len).and_then(|v| v.checked_add(4)) else {
            return false;
        }; // header + data + crc
        if rest.len() < end {
            return false;
        }
        if &rest[4..8] == b"IEND" {
            return rest.len() == end;
        }
        rest = &rest[end..];
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const TINY_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    fn tiny_png() -> Vec<u8> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(TINY_PNG)
            .unwrap()
    }

    #[test]
    fn dimensions_come_from_headers_without_a_decode() {
        assert_eq!(dimensions(&tiny_png()), Some((1, 1)));
        assert_eq!(dimensions(b"not an image"), None);
        assert_eq!(dimensions(b""), None);
    }

    #[test]
    fn png_passthrough_is_byte_identical() {
        let bytes = tiny_png();
        match to_png(&bytes) {
            Some(Cow::Borrowed(same)) => assert_eq!(same, bytes.as_slice()),
            Some(Cow::Owned(_)) => panic!("PNG input must pass through, not re-encode"),
            None => panic!("valid PNG must not fail"),
        }
    }

    #[test]
    fn jpeg_input_is_re_encoded_as_png() {
        let img = DynamicImage::new_rgb8(4, 3);
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .write_image(img.as_bytes(), 4, 3, img.color().into())
            .unwrap();
        assert!(!valid_png(&jpeg));
        let png = to_png(&jpeg).expect("decodable JPEG converts");
        assert_eq!(dimensions(&png), Some((4, 3)));
        assert_eq!(
            dimensions(&jpeg),
            Some((4, 3)),
            "headers work pre-convert too"
        );
    }

    #[test]
    fn corrupt_input_falls_back_to_none() {
        // Magic bytes alone must not qualify as PNG — a structurally
        // broken image sent as f=100 renders as nothing.
        let mut fake = vec![0x89u8, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
        assert_eq!(dimensions(&fake), None);
        assert_eq!(to_png(&fake), None);
        fake.truncate(4);
        assert_eq!(to_png(&fake), None);
        // A PNG with its IEND CRC chopped fails the structural walk,
        // but the decoder still salvages every pixel — so to_png
        // repairs it into a valid image instead of giving up.
        let mut truncated = tiny_png();
        truncated.truncate(truncated.len() - 4);
        assert!(!valid_png(&truncated));
        let repaired = to_png(&truncated).expect("decoder salvages a truncated tail");
        assert!(valid_png(&repaired));
    }

    #[test]
    fn rgba_pixels_roundtrip_through_png() {
        // 2x1: red, then semi-transparent green.
        let rgba = [255u8, 0, 0, 255, 0, 255, 0, 128];
        let png = from_rgba(2, 1, &rgba).expect("encodes");
        assert_eq!(dimensions(&png), Some((2, 1)));
        let decoded = ImageReader::new(Cursor::new(&png))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap()
            .to_rgba8();
        assert_eq!(decoded.as_raw().as_slice(), &rgba);
    }

    #[test]
    fn from_rgba_rejects_shape_mismatches() {
        assert_eq!(from_rgba(0, 1, &[]), None);
        assert_eq!(from_rgba(1, 0, &[]), None);
        // One byte short of the w*h*4 contract.
        assert_eq!(from_rgba(2, 2, &[0u8; 15]), None);
        // 2x2x4 would overflow u32 math if multiplied in u32 — it
        // must simply not match a short slice, not panic.
        assert_eq!(from_rgba(2, 2, &[0u8; 4]), None);
    }

    #[test]
    fn valid_png_walks_chunks_to_iend() {
        assert!(valid_png(&tiny_png()));
        // Bogus huge length must not panic the walker.
        let mut hostile = tiny_png()[..12].to_vec();
        hostile.extend_from_slice(&[0xFF; 4]);
        assert!(!valid_png(&hostile));
    }
}
