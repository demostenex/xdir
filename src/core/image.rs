use std::fs::File;
use std::io::{self, BufReader};
use std::path::Path;

use image::{ColorType, ImageDecoder, ImageError, ImageFormat, ImageReader};

/// Upper bound on the *compressed* file size a preview will even attempt to
/// open as an image. Cheap to check (`File::metadata`, no decode) and
/// catches pathological cases before any format/dimension inspection.
pub const MAX_IMAGE_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// Upper bound on either dimension, in pixels. Chosen well above anything a
/// real photo or screenshot needs, while still keeping `MAX_IMAGE_DIMENSION
/// * MAX_IMAGE_DIMENSION` (64 megapixels) a bounded, checked quantity long
/// before [`MAX_IMAGE_PIXELS`] rejects it.
pub const MAX_IMAGE_DIMENSION: u32 = 8192;

/// Upper bound on total pixel count (`width * height`), independent of
/// color type. Deliberately tighter than `MAX_IMAGE_DIMENSION²` — an
/// 8192×8192 image alone would be 64 megapixels, twice this limit — so a
/// merely-tall-and-thin image can't sneak past the per-axis check by
/// staying just under it on both axes. This ceiling is intentionally the
/// same regardless of color type: [`MAX_IMAGE_DECODED_BYTES`] (checked
/// separately, per color type, via [`exceeds_conversion_working_set`]) is
/// what actually lets a more expensive color type (e.g. 16-bit-per-channel)
/// get rejected well before it could reach this many pixels.
pub const MAX_IMAGE_PIXELS: u64 = 32 * 1024 * 1024;

/// Upper bound, in bytes, on the pixel-buffer working set this module will
/// let decode/conversion use at once — not just the final RGBA8 buffer, but
/// the native-color-type buffer the decoder itself produces, which may need
/// to coexist with the RGBA8 buffer during conversion (see
/// [`exceeds_conversion_working_set`]). At exactly `MAX_IMAGE_PIXELS`,
/// RGBA8 alone needs precisely 128 MiB, so a source that's already RGBA8
/// fits this exactly at the pixel ceiling; a source in any other color type
/// needs headroom below it to leave room for the separate RGBA8 buffer
/// built alongside it.
pub const MAX_IMAGE_DECODED_BYTES: u64 = 128 * 1024 * 1024;

/// Result of attempting to read `path` as an image preview.
#[derive(Debug, Clone, PartialEq)]
pub enum ImagePreview {
    /// Decoded, non-premultiplied RGBA8 pixels, ready to hand to a
    /// toolkit. `rgba.len() == width as usize * height as usize * 4` always
    /// holds — not asserted here, because it's already guaranteed by
    /// `image::RgbaImage` itself (an `ImageBuffer<Rgba<u8>, Vec<u8>>`: 4
    /// channels per pixel, and `into_raw()` returns exactly that backing
    /// `Vec` with no reslicing), not something this module computes or
    /// could get wrong independently.
    Image {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    /// Recognized as PNG or JPEG, but rejected by one of the limits above
    /// *before* the pixel buffer was allocated — decode was never
    /// attempted.
    TooLarge,
    /// The file's content isn't recognized as a supported image format at
    /// all (by signature, never by extension) — the caller should fall
    /// back to its own policy for the file (e.g. the text preview).
    NotAnImage,
    /// Recognized as PNG or JPEG (by signature or extension) but the
    /// content is malformed/corrupt and could not be decoded.
    Unsupported,
}

/// Reads at most a signature-sized prefix to identify the format, then —
/// only for a recognized, size-safe PNG/JPEG — decodes the full image.
///
/// Format identification is content-based *only*. `ImageReader::new`
/// (unlike [`ImageReader::open`]) never seeds a format guess from the
/// path's extension — it starts with no format opinion at all — so
/// [`ImageReader::with_guessed_format`]'s magic-byte sniff is the *only*
/// way `reader.format()` below can resolve to `Png`/`Jpeg`. A real PNG
/// named `photo.txt` is still recognized; a `.png` file that isn't valid
/// image data is never accepted just because of its name, and is left for
/// the caller's own (e.g. text) policy to classify.
///
/// Single pass, single file handle throughout: one `File::open`, its own
/// `metadata()` for the compressed size, and the same decoder built to
/// read dimensions (a cheap header parse, no pixel data) reused for the
/// full decode — this never opens or reads the file twice, and never
/// re-resolves the path.
///
/// Limits are enforced in cheapest-first order, and every arithmetic step
/// uses checked arithmetic — an overflow is treated exactly like exceeding
/// the limit (`TooLarge`), never silently wrapped:
/// 1. compressed file size ([`MAX_IMAGE_FILE_BYTES`]) — already-open file's
///    `metadata()`, no content I/O;
/// 2. each dimension ([`MAX_IMAGE_DIMENSION`]) — from the decoder's parsed
///    header, no pixel data read yet;
/// 3. total pixels ([`MAX_IMAGE_PIXELS`]), independent of color type;
/// 4. the conversion *working set* ([`MAX_IMAGE_DECODED_BYTES`],
///    [`exceeds_conversion_working_set`]) — the decoder's native buffer
///    size (`ImageDecoder::total_bytes`) together with the RGBA8 target
///    size, since [`image::DynamicImage::into_rgba8`] may need both to
///    coexist during conversion. None of these steps reads pixel data.
///
/// Only after every one of these passes does this call into the decoder
/// for actual pixel data.
pub fn read_image_preview(path: &Path) -> io::Result<ImagePreview> {
    let file = File::open(path)?;
    let file_len = file.metadata()?.len();

    let mut reader = ImageReader::new(BufReader::new(file)).with_guessed_format()?;

    if !matches!(
        reader.format(),
        Some(ImageFormat::Png) | Some(ImageFormat::Jpeg)
    ) {
        return Ok(ImagePreview::NotAnImage);
    }

    if file_len > MAX_IMAGE_FILE_BYTES {
        return Ok(ImagePreview::TooLarge);
    }

    // Best-effort extra guard inside the library itself. Per `image`'s own
    // docs this is non-strict for `max_alloc` and not every decoder
    // supports the strict width/height limits — so this is defense in
    // depth, never a substitute for the explicit checks below, which
    // remain this module's canonical policy regardless of what any given
    // decoder does with `Limits`.
    // `Limits` is `#[non_exhaustive]`, so it can't be struct-literal
    // constructed here even though every field is public — start from its
    // `Default` and set only what we care about.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_DECODED_BYTES);
    reader.limits(limits);

    let decoder = match reader.into_decoder() {
        Ok(decoder) => decoder,
        Err(ImageError::Limits(_)) => return Ok(ImagePreview::TooLarge),
        Err(_) => return Ok(ImagePreview::Unsupported),
    };

    let (width, height) = decoder.dimensions();
    if width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || exceeds_pixel_limits(width, height)
    {
        return Ok(ImagePreview::TooLarge);
    }

    let Some(target_rgba_bytes) = rgba_byte_size(width, height) else {
        return Ok(ImagePreview::TooLarge);
    };
    if exceeds_conversion_working_set(
        decoder.total_bytes(),
        target_rgba_bytes,
        decoder.color_type(),
    ) {
        return Ok(ImagePreview::TooLarge);
    }

    match image::DynamicImage::from_decoder(decoder) {
        Ok(dynamic) => {
            // `into_rgba8` (consuming) instead of `to_rgba8` (`&self`):
            // when the decoder already produced `DynamicImage::ImageRgba8`
            // — the common case for a PNG with an alpha channel — this
            // returns that buffer as-is, no clone. `to_rgba8` always
            // clones, even when the source is already RGBA8. Only a
            // source format that genuinely isn't RGBA8 (e.g. JPEG's
            // native RGB8, with no alpha channel to reuse) still pays for
            // an actual conversion here — unavoidable, since it's adding
            // a channel, not just re-viewing existing bytes. This is
            // exactly the distinction `exceeds_conversion_working_set`
            // makes above, before any of this runs.
            let rgba = dynamic.into_rgba8();
            let (width, height) = rgba.dimensions();
            Ok(ImagePreview::Image {
                width,
                height,
                rgba: rgba.into_raw(),
            })
        }
        Err(_) => Ok(ImagePreview::Unsupported),
    }
}

/// `width * height * 4` (RGBA8 byte size), or `None` on overflow. No real
/// PNG/JPEG header this crate parses can produce dimensions anywhere near
/// overflowing this — both `width` and `height` are `u32` — but checked
/// arithmetic is used regardless, so a future change to widen these types
/// can't turn this into a silent-wraparound bug.
fn rgba_byte_size(width: u32, height: u32) -> Option<u64> {
    u64::from(width)
        .checked_mul(u64::from(height))?
        .checked_mul(4)
}

/// Returns `true` if a `width`×`height` image would exceed [`MAX_IMAGE_PIXELS`]
/// (independent of color type) or what its RGBA8 form alone would need
/// against [`MAX_IMAGE_DECODED_BYTES`]. This is the color-type-agnostic,
/// cheap-first check; [`exceeds_conversion_working_set`] is the second,
/// more precise gate that additionally accounts for the decoder's native
/// buffer.
fn exceeds_pixel_limits(width: u32, height: u32) -> bool {
    let Some(pixels) = u64::from(width).checked_mul(u64::from(height)) else {
        return true;
    };
    if pixels > MAX_IMAGE_PIXELS {
        return true;
    }
    match rgba_byte_size(width, height) {
        Some(bytes) => bytes > MAX_IMAGE_DECODED_BYTES,
        None => true,
    }
}

/// Returns `true` if decoding into `source_color_type`'s native buffer and
/// then converting to RGBA8 could need more than [`MAX_IMAGE_DECODED_BYTES`]
/// of pixel-buffer memory at once.
///
/// `DynamicImage::into_rgba8` (see [`read_image_preview`]) returns the
/// decoder's own buffer unchanged when it's already `ColorType::Rgba8` —
/// source and target are the *same* allocation then, so the working set is
/// whichever of the two sizes is larger (in practice equal). For any other
/// source color type, a real conversion happens: the native buffer and the
/// newly-allocated RGBA8 buffer can coexist for the duration of that
/// conversion, so the working set is their sum. `checked_add` makes an
/// overflow (unreachable for any real decoder's `total_bytes()` plus a
/// `u32`-dimension-bounded RGBA8 size, but never assumed) reject exactly
/// like exceeding the limit.
fn exceeds_conversion_working_set(
    source_decoded_bytes: u64,
    target_rgba_bytes: u64,
    source_color_type: ColorType,
) -> bool {
    let working_bytes = if source_color_type == ColorType::Rgba8 {
        source_decoded_bytes.max(target_rgba_bytes)
    } else {
        match source_decoded_bytes.checked_add(target_rgba_bytes) {
            Some(sum) => sum,
            None => return true,
        }
    };
    working_bytes > MAX_IMAGE_DECODED_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::fs;

    fn write_png(path: &Path, width: u32, height: u32, pixel: image::Rgba<u8>) {
        let img = image::RgbaImage::from_pixel(width, height, pixel);
        img.save_with_format(path, ImageFormat::Png).unwrap();
    }

    fn write_jpeg(path: &Path, width: u32, height: u32, pixel: image::Rgb<u8>) {
        let img = image::RgbImage::from_pixel(width, height, pixel);
        img.save_with_format(path, ImageFormat::Jpeg).unwrap();
    }

    #[test]
    fn small_valid_png_is_image() {
        let dir = TempDir::new();
        let path = dir.path().join("small.png");
        write_png(&path, 2, 2, image::Rgba([10, 20, 30, 255]));

        let preview = read_image_preview(&path).unwrap();

        match preview {
            ImagePreview::Image {
                width,
                height,
                rgba,
            } => {
                assert_eq!((width, height), (2, 2));
                assert_eq!(rgba.len(), 2 * 2 * 4);
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn small_valid_jpeg_is_image() {
        let dir = TempDir::new();
        let path = dir.path().join("small.jpg");
        write_jpeg(&path, 16, 16, image::Rgb([200, 100, 50]));

        let preview = read_image_preview(&path).unwrap();

        match preview {
            ImagePreview::Image {
                width,
                height,
                rgba,
            } => {
                assert_eq!((width, height), (16, 16));
                assert_eq!(rgba.len(), 16 * 16 * 4);
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn dimensions_are_stored_correctly_for_a_non_square_image() {
        let dir = TempDir::new();
        let path = dir.path().join("rect.png");
        write_png(&path, 5, 3, image::Rgba([0, 0, 0, 255]));

        let preview = read_image_preview(&path).unwrap();

        match preview {
            ImagePreview::Image { width, height, .. } => assert_eq!((width, height), (5, 3)),
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn png_with_alpha_is_accepted_and_alpha_is_preserved() {
        let dir = TempDir::new();
        let path = dir.path().join("alpha.png");
        write_png(&path, 1, 1, image::Rgba([255, 0, 0, 128]));

        let preview = read_image_preview(&path).unwrap();

        match preview {
            ImagePreview::Image { rgba, .. } => {
                assert_eq!(&rgba[..4], &[255, 0, 0, 128]);
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn png_extension_with_invalid_content_is_not_an_image() {
        let dir = TempDir::new();
        let path = dir.path().join("fake.png");
        // Not a PNG signature, and not valid UTF-8 either (so this also
        // resolves to `Unsupported` if a caller falls through to text
        // policy), matching a genuinely garbage file that merely happens
        // to be named `.png`.
        fs::write(&path, [0x00, 0x01, 0x02, 0x80, 0xff, 0xfe]).unwrap();

        let preview = read_image_preview(&path).unwrap();

        assert!(!matches!(preview, ImagePreview::Image { .. }));
    }

    #[test]
    fn valid_png_with_wrong_extension_is_still_image() {
        let dir = TempDir::new();
        let path = dir.path().join("photo.txt");
        write_png(&path, 3, 3, image::Rgba([1, 2, 3, 255]));

        let preview = read_image_preview(&path).unwrap();

        assert!(matches!(preview, ImagePreview::Image { .. }));
    }

    #[test]
    fn text_file_named_png_is_not_an_image() {
        let dir = TempDir::new();
        let path = dir.path().join("notes.png");
        fs::write(&path, "just some text\n").unwrap();

        let preview = read_image_preview(&path).unwrap();

        assert_eq!(preview, ImagePreview::NotAnImage);
    }

    #[test]
    fn text_file_named_jpg_is_not_an_image() {
        let dir = TempDir::new();
        let path = dir.path().join("notes.jpg");
        fs::write(&path, "just some text\n").unwrap();

        let preview = read_image_preview(&path).unwrap();

        assert_eq!(preview, ImagePreview::NotAnImage);
    }

    #[test]
    fn dimension_over_the_limit_is_rejected_before_full_decode() {
        let dir = TempDir::new();
        let path = dir.path().join("too-wide.png");
        // 1 pixel tall keeps this fixture cheap to construct even though
        // its width alone already exceeds MAX_IMAGE_DIMENSION.
        write_png(
            &path,
            MAX_IMAGE_DIMENSION + 1,
            1,
            image::Rgba([0, 0, 0, 255]),
        );

        let preview = read_image_preview(&path).unwrap();

        assert_eq!(preview, ImagePreview::TooLarge);
    }

    #[test]
    fn exceeds_pixel_limits_catches_a_thin_image_within_both_per_axis_limits() {
        // Both axes individually fit under MAX_IMAGE_DIMENSION (8192), but
        // their product (~33.6 million pixels) exceeds MAX_IMAGE_PIXELS
        // (33,554,432) — pure arithmetic against the exported function, no
        // multi-hundred-megabyte fixture allocated to prove it.
        let width = 8000;
        let height = 4200;
        assert!(width <= MAX_IMAGE_DIMENSION && height <= MAX_IMAGE_DIMENSION);
        assert!(exceeds_pixel_limits(width, height));
    }

    #[test]
    fn exceeds_pixel_limits_never_overflows_on_extreme_synthetic_input() {
        // No real PNG/JPEG header parsed by this crate can produce
        // dimensions anywhere near `u32::MAX`, but the arithmetic must
        // still never panic or silently wrap if it somehow did.
        assert!(exceeds_pixel_limits(u32::MAX, u32::MAX));
        assert!(exceeds_pixel_limits(u32::MAX, 1));
        assert!(!exceeds_pixel_limits(1, 1));
        assert!(!exceeds_pixel_limits(MAX_IMAGE_DIMENSION, 1));
    }

    // --- exceeds_conversion_working_set: pure-logic tests, no real image
    // decoded. Byte counts below are derived directly from real dimensions
    // (documented per test) so they're not arbitrary, but no pixel buffer
    // of that size is ever allocated to run them.

    #[test]
    fn small_rgba8_is_within_decode_memory_limit() {
        // 2x2 RGBA8: 16 bytes either way.
        assert!(!exceeds_conversion_working_set(16, 16, ColorType::Rgba8));
    }

    #[test]
    fn four_k_rgb8_is_within_decode_memory_limit() {
        // 3840x2160 = 8,294,400 px. RGB8 source = *3 = 24,883,200 B
        // (~23.7 MiB). RGBA8 target = *4 = 33,177,600 B (~31.6 MiB). Sum
        // ~58 MiB, comfortably under MAX_IMAGE_DECODED_BYTES (128 MiB).
        let pixels: u64 = 3840 * 2160;
        assert!(!exceeds_conversion_working_set(
            pixels * 3,
            pixels * 4,
            ColorType::Rgb8
        ));
    }

    #[test]
    fn rgba8_at_allowed_pixel_ceiling_obeys_memory_limit() {
        // Exactly MAX_IMAGE_PIXELS: RGBA8 source and target are the same
        // 134,217,728-byte (128 MiB) buffer — right at the limit, not
        // over it (`>`, not `>=`).
        let bytes = MAX_IMAGE_PIXELS * 4;
        assert_eq!(bytes, MAX_IMAGE_DECODED_BYTES);
        assert!(!exceeds_conversion_working_set(
            bytes,
            bytes,
            ColorType::Rgba8
        ));
    }

    #[test]
    fn rgb8_that_passes_pixel_limit_but_exceeds_conversion_memory_is_rejected() {
        // The critical case: pixels at exactly MAX_IMAGE_PIXELS (so the
        // pixel-count check alone would pass), RGB8 source (~96 MiB) and
        // RGBA8 target (exactly 128 MiB) each individually plausible, but
        // their *sum* (~224 MiB) exceeds MAX_IMAGE_DECODED_BYTES.
        let source = MAX_IMAGE_PIXELS * 3;
        let target = MAX_IMAGE_PIXELS * 4;
        assert!(!exceeds_pixel_limits(8192, 4096)); // 8192*4096 == MAX_IMAGE_PIXELS
        assert!(exceeds_conversion_working_set(
            source,
            target,
            ColorType::Rgb8
        ));
    }

    #[test]
    fn rgba16_source_over_memory_limit_is_rejected() {
        // 4096x4096 = 16,777,216 px. Rgba16 source = *8 = 134,217,728 B
        // (exactly 128 MiB) *alone*; adding the RGBA8 target (*4 = 64 MiB)
        // pushes the sum well past MAX_IMAGE_DECODED_BYTES, even though
        // neither buffer alone is absurd.
        let pixels: u64 = 4096 * 4096;
        assert!(exceeds_conversion_working_set(
            pixels * 8,
            pixels * 4,
            ColorType::Rgba16
        ));
    }

    #[test]
    fn conversion_working_set_arithmetic_overflow_is_rejected() {
        assert!(exceeds_conversion_working_set(
            u64::MAX,
            u64::MAX,
            ColorType::Rgb8
        ));
        assert!(rgba_byte_size(u32::MAX, u32::MAX).is_none());
    }

    #[test]
    fn corrupted_image_data_is_unsupported() {
        let dir = TempDir::new();
        let path = dir.path().join("corrupt.jpg");
        // A real JPEG signature (so format detection succeeds and
        // dimensions may even parse) followed by garbage instead of valid
        // scan data.
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
        bytes.extend(std::iter::repeat_n(0u8, 64));
        fs::write(&path, &bytes).unwrap();

        let preview = read_image_preview(&path).unwrap();

        assert!(matches!(
            preview,
            ImagePreview::Unsupported | ImagePreview::NotAnImage
        ));
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let dir = TempDir::new();
        let path = dir.path().join("does-not-exist.png");

        assert!(read_image_preview(&path).is_err());
    }

    #[test]
    fn plain_text_file_is_not_an_image() {
        let dir = TempDir::new();
        let path = dir.path().join("notes.txt");
        fs::write(&path, "just some text\n").unwrap();

        let preview = read_image_preview(&path).unwrap();

        assert_eq!(preview, ImagePreview::NotAnImage);
    }
}
