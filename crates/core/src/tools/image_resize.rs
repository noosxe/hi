//! Image resizing for `read`'s attachments — pi's ladder
//! (`image-resize-core.js`), Rust edition (AD-0009).
//!
//! No provider exposes image limits on the wire; the documented ones
//! disagree with each other and some depend on request context. So
//! like pi, we don't try to know: one conservative profile — 2000×2000,
//! 4.5 MB of base64 payload, JPEG q80 — chosen to sit under every
//! mainstream provider, applied before an image enters conversation
//! history. Deterministic input→output bytes keep the AD-0007 prefix
//! invariant intact: history stores the resized attachment, and every
//! replay re-derives the same carrier.

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, Limits};
use std::io::Cursor;

/// pi's dimension ceiling — Anthropic's many-image limit, OpenAI's
/// classic guidance, and past that models rasterize anyway.
pub(crate) const MAX_IMAGE_DIMENSION: u32 = 2000;

/// pi's byte ceiling: 4.5 MB of base64 payload — headroom below the
/// tightest per-image cap (5 MB base64 on Bedrock/Google-Cloud-hosted
/// models). The budget counts the encoded bytes, because that is what
/// rides the wire.
pub(crate) const MAX_IMAGE_ENCODED_BYTES: usize = 4_718_592; // 4.5 * 1024 * 1024

/// JPEG qualities tried at each size, pi's set, first match wins
/// (PNG first, then these — a PNG that fits beats a lossy JPEG).
const JPEG_QUALITIES: [u8; 5] = [80, 85, 70, 55, 40];

/// Raw-input guard, hi's addition: the ladder can shrink anything
/// decodable, but reading a 10 GB file into memory first is not a
/// feature. Generous on purpose — this is the OOM guard, not a
/// provider limit; the ladder is the size policy.
pub(crate) const MAX_IMAGE_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Decode guard: past this, pixels would only be scaled away — and a
/// hostile PNG header claiming a million-pixel width must not become a
/// 4 TB allocation. Matches the strictest provider maximum
/// (Anthropic 8000×8000) with headroom for 5K screenshots.
const MAX_DECODE_DIMENSION: u32 = 8192;

/// A processed attachment: the bytes that will be stored on the
/// `ToolResult` (post-resize — the ladder runs once, before history),
/// plus what happened, for the note.
#[derive(Debug)]
pub(crate) struct ProcessedImage {
    pub bytes: Vec<u8>,
    pub mime_type: &'static str,
    /// `(original_w, original_h, final_w, final_h)` when dimensions
    /// changed.
    pub resized: Option<(u32, u32, u32, u32)>,
    /// Source format when the output format differs from it.
    pub converted_from: Option<&'static str>,
}

#[derive(Debug)]
pub(crate) enum ProcessError {
    /// Raw-input guard, before any decode.
    TooLarge(u64),
    /// Magic bytes matched but the decoder disagrees — a corrupt or
    /// truncated file.
    Decode(String),
    /// The ladder hit 1×1 and the image still doesn't fit the budget.
    Unshrinkable,
}

impl ProcessError {
    /// The honest reason, in the tool-note voice.
    pub(crate) fn note(&self) -> String {
        match self {
            ProcessError::TooLarge(size) => format!(
                "{} exceeds the {} processing limit",
                crate::tools::read::human_size(*size),
                crate::tools::read::human_size(MAX_IMAGE_FILE_BYTES)
            ),
            ProcessError::Decode(err) => format!("image could not be decoded ({err})"),
            ProcessError::Unshrinkable => {
                "could not be resized below the inline image size limit".into()
            }
        }
    }
}

/// The encoded-size arithmetic pi uses: base64 inflates by 4/3.
fn base64_size(bytes_len: usize) -> usize {
    bytes_len.div_ceil(3) * 4
}

/// `process` at production limits.
pub(crate) fn process(bytes: &[u8], mime: &'static str) -> Result<ProcessedImage, ProcessError> {
    process_with(
        bytes,
        mime,
        MAX_IMAGE_DIMENSION,
        MAX_IMAGE_ENCODED_BYTES,
        MAX_IMAGE_FILE_BYTES,
    )
}

/// The ladder, parameterized for tests. Mirrors
/// `resizeImageInProcess` step for step:
///
/// 1. Already within dimensions AND encoded budget → original bytes,
///    untouched (a small screenshot is never re-encoded).
/// 2. Otherwise, at each size (starting fit-within-`max_dimension`):
///    try PNG, then JPEG at [`JPEG_QUALITIES`] — first candidate under
///    the budget wins.
/// 3. Still over? Shrink by 0.75× (floor 1×1) and repeat.
/// 4. 1×1 and still over → [`ProcessError::Unshrinkable`].
fn process_with(
    bytes: &[u8],
    mime: &'static str,
    max_dimension: u32,
    max_encoded_bytes: usize,
    max_file_bytes: u64,
) -> Result<ProcessedImage, ProcessError> {
    if bytes.len() as u64 > max_file_bytes {
        return Err(ProcessError::TooLarge(bytes.len() as u64));
    }
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|err| ProcessError::Decode(err.to_string()))?;
    let mut limits = Limits::default(); // #[non_exhaustive]: built by assignment
    limits.max_image_width = Some(MAX_DECODE_DIMENSION);
    limits.max_image_height = Some(MAX_DECODE_DIMENSION);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .map_err(|err| ProcessError::Decode(err.to_string()))?;
    // Phone photos lie about up: EXIF orientation is applied before
    // anything else, exactly like pi's `applyExifOrientation`.
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img =
        DynamicImage::from_decoder(decoder).map_err(|err| ProcessError::Decode(err.to_string()))?;
    img.apply_orientation(orientation);
    let (original_width, original_height) = (img.width(), img.height());

    // Step 1: within limits means untouched — byte-identical, original
    // mime type, no re-encode of an already-fitting file.
    if original_width <= max_dimension
        && original_height <= max_dimension
        && base64_size(bytes.len()) < max_encoded_bytes
    {
        return Ok(ProcessedImage {
            bytes: bytes.to_vec(),
            mime_type: mime,
            resized: None,
            converted_from: None,
        });
    }

    // Step 2: initial target, aspect-preserving fit.
    let mut width = original_width.min(max_dimension);
    let mut height = original_height.min(max_dimension);
    if width != original_width {
        height = (original_height * max_dimension) / original_width;
    } else if height != original_height {
        width = (original_width * max_dimension) / original_height;
    }

    // Steps 2–4: the candidate loop.
    loop {
        let resized = img.resize_exact(width, height, FilterType::Lanczos3);
        let mut candidates: Vec<(Vec<u8>, &'static str)> = Vec::new();
        // `write_to` takes its writer by value, so PNG goes through an
        // encoder that borrows the buffer back.
        let mut png = Vec::new();
        if resized
            .write_with_encoder(PngEncoder::new(&mut png))
            .is_ok()
        {
            candidates.push((png, "image/png"));
        }
        for quality in JPEG_QUALITIES {
            let mut buf = Vec::new();
            if resized
                .write_with_encoder(JpegEncoder::new_with_quality(&mut buf, quality))
                .is_ok()
            {
                candidates.push((buf, "image/jpeg"));
            }
        }
        for (encoded, candidate_mime) in candidates {
            if base64_size(encoded.len()) < max_encoded_bytes {
                let converted_from = (candidate_mime != mime).then_some(mime);
                let resized_note = (width != original_width || height != original_height)
                    .then_some((original_width, original_height, width, height));
                return Ok(ProcessedImage {
                    bytes: encoded,
                    mime_type: candidate_mime,
                    resized: resized_note,
                    converted_from,
                });
            }
        }
        if width == 1 && height == 1 {
            return Err(ProcessError::Unshrinkable);
        }
        let next_width = (width / 4 * 3).max(1);
        let next_height = (height / 4 * 3).max(1);
        if next_width == width && next_height == height {
            return Err(ProcessError::Unshrinkable);
        }
        width = next_width;
        height = next_height;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real 1×1 transparent PNG — the fixture for "already fits".
    const TINY_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    fn tiny_png() -> Vec<u8> {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        STANDARD.decode(TINY_PNG).unwrap()
    }

    /// Dithered gray: every pixel is 128 or 129, unpredictable — PNG
    /// can't compress it, JPEG's DCT quantizes it away. The reliable
    /// way to force the transcode path. Fixed-seed xorshift keeps the
    /// ladder's path stable run to run.
    fn dithered_png(width: u32, height: u32) -> Vec<u8> {
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        let mut raw = Vec::with_capacity((width * height * 3) as usize);
        for _ in 0..width * height {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let v = 128 + (seed & 1) as u8;
            raw.extend_from_slice(&[v, v, v]);
        }
        let buffer = image::ImageBuffer::from_raw(width, height, raw).unwrap();
        let img = DynamicImage::ImageRgb8(buffer);
        let mut buf = Vec::new();
        img.write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    #[test]
    fn small_fitting_images_stay_byte_identical() {
        let bytes = tiny_png();
        let out = process(&bytes, "image/png").unwrap();
        assert_eq!(out.bytes, bytes, "a fitting image is never re-encoded");
        assert_eq!(out.mime_type, "image/png");
        assert!(out.resized.is_none());
        assert!(out.converted_from.is_none());
    }

    #[test]
    fn oversized_dimensions_are_scaled_to_fit() {
        // 2200×1500 solid color: decode-cheap, over the dimension cap.
        let img = DynamicImage::new_rgb8(2200, 1500);
        let mut bytes = Vec::new();
        img.write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();

        let out = process(&bytes, "image/png").unwrap();

        let (ow, oh, w, h) = out.resized.expect("dimensions changed");
        assert_eq!((ow, oh), (2200, 1500));
        assert_eq!((w, h), (2000, 1363)); // aspect kept: 1500*2000/2200
        assert_eq!(out.mime_type, "image/png");
        assert!(out.converted_from.is_none());
    }

    #[test]
    fn tight_budgets_transcode_to_jpeg_before_shrinking() {
        // Dithered gray at the dimension cap: the PNG candidate is far
        // over a 1 MB budget, the q80 JPEG is not — the transcode path.
        let bytes = dithered_png(2000, 2000);
        let out = process_with(&bytes, "image/png", 2000, 1024 * 1024, u64::MAX).unwrap();
        assert_eq!(out.mime_type, "image/jpeg");
        assert_eq!(out.converted_from, Some("image/png"));
        // The budget bit, whatever size it took.
        assert!(base64_size(out.bytes.len()) < 1024 * 1024);
    }

    #[test]
    fn budgets_below_1x1_report_unshrinkable() {
        let bytes = tiny_png(); // 70 bytes -> 96 base64: budget 90 is past the no-op check
        let err = process_with(&bytes, "image/png", 2000, 90, u64::MAX).unwrap_err();
        assert!(matches!(err, ProcessError::Unshrinkable));
        assert!(err.note().contains("could not be resized below"));
    }

    #[test]
    fn raw_input_guard_rejects_before_decode() {
        let bytes = vec![0u8; 65 * 1024 * 1024];
        let err = process(&bytes, "image/png").unwrap_err();
        assert!(matches!(err, ProcessError::TooLarge(_)));
        assert!(err.note().contains("processing limit"));
    }

    #[test]
    fn corrupt_magic_bytes_report_decode_failure() {
        // PNG magic followed by garbage: the sniff passes, decode can't.
        let bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
        let err = process(&bytes, "image/png").unwrap_err();
        assert!(matches!(err, ProcessError::Decode(_)));
        assert!(err.note().contains("could not be decoded"));
    }
}
