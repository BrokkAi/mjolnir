//! Decode and constrain user supplied images for controller and client APIs.

use std::io::{self, Cursor, Write};

use anyhow::{Context, Result, anyhow, bail};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::metadata::Orientation;
use image::{
    ColorType, DynamicImage, GenericImageView, ImageDecoder, ImageFormat, ImageReader, Limits,
    RgbaImage,
};

const MAX_INPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_DECODED_BYTES: u64 = 256 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = hel::hel_attachment::MAX_IMAGE_BYTES;
const MAX_JPEG_DIMENSION: u32 = 65_535;
const JPEG_QUALITIES: [u8; 3] = [90, 85, 80];

/// An image encoded in a bounded, browser-friendly representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptimizedImage {
    pub bytes: Vec<u8>,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
}

/// Decode and optimize a JPEG, PNG, or WebP image.
///
/// Images larger than the output budget are recompressed and progressively resized. EXIF
/// orientation is applied before deciding the output dimensions.
pub fn optimize_image(bytes: &[u8]) -> Result<OptimizedImage> {
    if bytes.len() > MAX_INPUT_BYTES {
        bail!("image input exceeds the 64 MiB limit");
    }

    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("guess image format")?;
    let format = reader
        .format()
        .ok_or_else(|| anyhow!("unsupported image format"))?;
    if !matches!(
        format,
        ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP
    ) {
        bail!("unsupported image format (expected JPEG, PNG, or WebP)");
    }

    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().context("create image decoder")?;
    let orientation = decoder.orientation().context("read image orientation")?;
    let (width, height) = decoder.dimensions();
    let decoded_bytes = decoder.total_bytes();
    if decoded_bytes > MAX_DECODED_BYTES {
        bail!("decoded image exceeds the 256 MiB limit");
    }

    let mut image = DynamicImage::from_decoder(decoder).context("decode image")?;
    image.apply_orientation(orientation);

    // A normal JPEG or PNG can keep its original metadata and compression when it already fits.
    if orientation == Orientation::NoTransforms
        && matches!(format, ImageFormat::Jpeg | ImageFormat::Png)
        && bytes.len() <= MAX_OUTPUT_BYTES
    {
        let (width, height) = image.dimensions();
        return Ok(OptimizedImage {
            bytes: clone_bytes(bytes)?,
            mime_type: format.to_mime_type().to_owned(),
            width,
            height,
        });
    }

    // Keep this check tied to the decoder's advertised representation. It catches dimensions
    // whose multiplication would overflow before any image buffer is allocated.
    let _ = checked_image_bytes(width, height, image.color())?;
    optimize_dynamic_image(image)
}

/// Optimize an 8-bit RGBA image supplied as raw pixels.
pub fn optimize_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<OptimizedImage> {
    if width == 0 || height == 0 {
        bail!("image dimensions must be non-zero");
    }
    let expected_len = checked_rgba_len(width, height)?;
    if rgba.len() != expected_len {
        bail!("RGBA buffer length does not match image dimensions");
    }

    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(rgba.len())
        .map_err(|error| anyhow!("allocate RGBA image: {error}"))?;
    pixels.extend_from_slice(rgba);
    let image = RgbaImage::from_raw(width, height, pixels)
        .ok_or_else(|| anyhow!("invalid RGBA image dimensions"))?;
    optimize_dynamic_image(DynamicImage::ImageRgba8(image))
}

fn optimize_dynamic_image(mut image: DynamicImage) -> Result<OptimizedImage> {
    let transparent = contains_transparency(&image);

    loop {
        let (width, height) = image.dimensions();

        // JPEG dimensions are limited to a 16-bit unsigned value. Resize before asking the
        // encoder to handle an otherwise valid PNG/WebP with larger dimensions.
        if !transparent && (width > MAX_JPEG_DIMENSION || height > MAX_JPEG_DIMENSION) {
            let (next_width, next_height) =
                fit_dimensions(width, height, MAX_JPEG_DIMENSION, MAX_JPEG_DIMENSION);
            image = resize_checked(image, next_width, next_height)?;
            continue;
        }

        if transparent {
            if let Some(bytes) = encode_png(&image)? {
                return Ok(OptimizedImage {
                    bytes,
                    mime_type: "image/png".to_owned(),
                    width,
                    height,
                });
            }
        } else {
            for quality in JPEG_QUALITIES {
                if let Some(bytes) = encode_jpeg(&image, quality)? {
                    return Ok(OptimizedImage {
                        bytes,
                        mime_type: "image/jpeg".to_owned(),
                        width,
                        height,
                    });
                }
            }
        }

        let (next_width, next_height) = reduced_dimensions(width, height);
        if (next_width, next_height) == (width, height) {
            bail!("could not encode image within the 700 KiB output limit");
        }
        image = resize_checked(image, next_width, next_height)?;
    }
}

fn encode_png(image: &DynamicImage) -> Result<Option<Vec<u8>>> {
    let mut writer = LimitedWriter::new(MAX_OUTPUT_BYTES);
    let result = image.write_with_encoder(PngEncoder::new_with_quality(
        &mut writer,
        CompressionType::Best,
        FilterType::Adaptive,
    ));
    if writer.too_large {
        return Ok(None);
    }
    result.context("encode PNG")?;
    Ok(Some(writer.into_inner()))
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> Result<Option<Vec<u8>>> {
    let mut writer = LimitedWriter::new(MAX_OUTPUT_BYTES);
    let mut encoder = JpegEncoder::new_with_quality(&mut writer, quality);
    let result = match image {
        // Calling encode_image directly keeps RGBA inputs from making a second full-size RGB
        // allocation. The encoder ignores alpha when it receives an opaque image.
        DynamicImage::ImageLuma8(buffer) => encoder.encode_image(buffer),
        DynamicImage::ImageLumaA8(buffer) => encoder.encode_image(buffer),
        DynamicImage::ImageRgb8(buffer) => encoder.encode_image(buffer),
        DynamicImage::ImageRgba8(buffer) => encoder.encode_image(buffer),
        _ => image.write_with_encoder(encoder),
    };
    if writer.too_large {
        return Ok(None);
    }
    result.context("encode JPEG")?;
    Ok(Some(writer.into_inner()))
}

fn contains_transparency(image: &DynamicImage) -> bool {
    match image {
        DynamicImage::ImageLumaA8(buffer) => buffer.pixels().any(|pixel| pixel[1] != u8::MAX),
        DynamicImage::ImageRgba8(buffer) => buffer.pixels().any(|pixel| pixel[3] != u8::MAX),
        DynamicImage::ImageLumaA16(buffer) => buffer.pixels().any(|pixel| pixel[1] != u16::MAX),
        DynamicImage::ImageRgba16(buffer) => buffer.pixels().any(|pixel| pixel[3] != u16::MAX),
        DynamicImage::ImageRgba32F(buffer) => buffer.pixels().any(|pixel| pixel[3] != 1.0),
        _ => false,
    }
}

fn resize_checked(image: DynamicImage, width: u32, height: u32) -> Result<DynamicImage> {
    if width == 0 || height == 0 {
        bail!("image dimensions must be non-zero");
    }
    let current_bytes = checked_image_bytes(image.width(), image.height(), image.color())?;
    let next_bytes = checked_image_bytes(width, height, image.color())?;
    if current_bytes
        .checked_add(next_bytes)
        .is_none_or(|bytes| bytes > MAX_DECODED_BYTES.saturating_mul(2))
    {
        bail!("image resize would exceed the memory limit");
    }
    Ok(image.resize_exact(width, height, image::imageops::FilterType::Lanczos3))
}

fn reduced_dimensions(width: u32, height: u32) -> (u32, u32) {
    let next_width = ((u64::from(width) * 4) / 5).max(1) as u32;
    let next_height = ((u64::from(height) * 4) / 5).max(1) as u32;
    if (next_width, next_height) == (width, height) {
        if width > height {
            (width.saturating_sub(1), height)
        } else {
            (width, height.saturating_sub(1))
        }
    } else {
        (next_width, next_height)
    }
}

fn fit_dimensions(width: u32, height: u32, max_width: u32, max_height: u32) -> (u32, u32) {
    if width <= max_width && height <= max_height {
        return (width, height);
    }

    let width_scale = u64::from(max_width) * u64::from(height);
    let height_scale = u64::from(max_height) * u64::from(width);
    let (scale_numerator, scale_denominator) = if width_scale <= height_scale {
        (u64::from(max_width), u64::from(width))
    } else {
        (u64::from(max_height), u64::from(height))
    };
    let next_width = (u64::from(width) * scale_numerator / scale_denominator).max(1) as u32;
    let next_height = (u64::from(height) * scale_numerator / scale_denominator).max(1) as u32;
    (next_width, next_height)
}

fn checked_rgba_len(width: u32, height: u32) -> Result<usize> {
    let bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| anyhow!("RGBA image dimensions overflow"))?;
    if bytes > MAX_DECODED_BYTES {
        bail!("decoded image exceeds the 256 MiB limit");
    }
    usize::try_from(bytes).map_err(|_| anyhow!("RGBA image is too large for this platform"))
}

fn checked_image_bytes(width: u32, height: u32, color: ColorType) -> Result<u64> {
    let bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(u64::from(color.bytes_per_pixel())))
        .ok_or_else(|| anyhow!("image dimensions overflow"))?;
    if bytes > MAX_DECODED_BYTES {
        bail!("decoded image exceeds the 256 MiB limit");
    }
    Ok(bytes)
}

fn clone_bytes(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut cloned = Vec::new();
    cloned
        .try_reserve_exact(bytes.len())
        .map_err(|error| anyhow!("allocate output image: {error}"))?;
    cloned.extend_from_slice(bytes);
    Ok(cloned)
}

struct LimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
    too_large: bool,
}

impl LimitedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            too_large: false,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(new_len) = self.bytes.len().checked_add(bytes.len()) else {
            self.too_large = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "encoded image exceeds size limit",
            ));
        };
        if new_len > self.limit {
            self.too_large = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "encoded image exceeds size limit",
            ));
        }
        self.bytes
            .try_reserve(bytes.len())
            .map_err(|error| io::Error::other(format!("allocate encoded image: {error}")))?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(width: u32, height: u32) -> Vec<u8> {
        let mut state = 0x1234_5678_u32;
        let mut bytes = vec![0; checked_rgba_len(width, height).unwrap()];
        for byte in &mut bytes {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *byte = (state >> 24) as u8;
        }
        for pixel in bytes.chunks_exact_mut(4) {
            pixel[3] = u8::MAX;
        }
        bytes
    }

    #[test]
    fn corrupt_input_is_rejected() {
        assert!(optimize_image(b"not an image").is_err());
    }

    #[test]
    fn oversized_input_is_rejected_before_decoding() {
        let bytes = vec![0; MAX_INPUT_BYTES + 1];
        let error = optimize_image(&bytes).unwrap_err().to_string();
        assert!(error.contains("64 MiB"));
    }

    #[test]
    fn transparent_rgba_stays_png() {
        let mut rgba = vec![0; 2 * 2 * 4];
        rgba.chunks_exact_mut(4).for_each(|pixel| pixel[3] = 128);
        let optimized = optimize_rgba(2, 2, &rgba).unwrap();
        assert_eq!(optimized.mime_type, "image/png");
        assert!(optimized.bytes.len() <= MAX_OUTPUT_BYTES);
        let decoder = ImageReader::new(Cursor::new(&optimized.bytes))
            .with_guessed_format()
            .unwrap()
            .into_decoder()
            .unwrap();
        assert_eq!(decoder.dimensions(), (2, 2));
        assert!(decoder.color_type().has_alpha());
    }

    #[test]
    fn noisy_image_is_resized_to_fit_output_budget() {
        let width = 1_600;
        let height = 1_200;
        let optimized = optimize_rgba(width, height, &noise(width, height)).unwrap();
        assert_eq!(optimized.mime_type, "image/jpeg");
        assert!(optimized.bytes.len() <= MAX_OUTPUT_BYTES);
        assert!(optimized.width < width || optimized.height < height);
    }

    #[test]
    fn rgba_length_must_match_dimensions() {
        assert!(optimize_rgba(2, 2, &[0; 3]).is_err());
    }
}
