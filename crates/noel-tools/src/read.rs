//! The `read` tool (upstream `read.ts`): text with line and byte limits, and
//! images as attachments.

use std::io::Cursor;
use std::path::PathBuf;

use base64::Engine as _;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};
use noel_env::{Context, ExecutionEnv};

use crate::paths::resolve_read_path;
use crate::truncate::{format_size, truncate_head, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};
use crate::{node_error_code, ToolError, ToolImage, ToolResult, ToolRuntimeOptions};

pub(crate) async fn read_tool(
    env: &dyn ExecutionEnv,
    input_path: &str,
    offset: Option<u64>,
    limit: Option<u64>,
    options: &ToolRuntimeOptions,
    cx: &Context,
) -> Result<ToolResult, ToolError> {
    let path = resolve_read_path(env, input_path, cx).await?;
    // The common failures are worded as upstream's (Node's) are.
    let bytes = env.read_binary_file(&path, cx).await.map_err(|error| {
        match node_error_code(error.code) {
            Some("ENOENT") => ToolError::Failed(format!(
                "ENOENT: no such file or directory, access '{path}'"
            )),
            Some("EACCES") => {
                ToolError::Failed(format!("EACCES: permission denied, access '{path}'"))
            }
            Some("EISDIR") => {
                ToolError::Failed("EISDIR: illegal operation on a directory, read".to_string())
            }
            _ => ToolError::Io {
                path: PathBuf::from(&path),
                source: error.to_io_error(),
            },
        }
    })?;
    if let Some(mime_type) = image_mime_type(&bytes) {
        return Ok(read_image(&bytes, mime_type, options));
    }

    let text = String::from_utf8_lossy(&bytes);
    // A final newline is followed by one more, empty, line.
    let lines = text.split('\n').collect::<Vec<_>>();
    let start = offset.map_or(0, |offset| offset.saturating_sub(1) as usize);
    if start >= lines.len() {
        return Err(ToolError::Failed(format!(
            "Offset {} is beyond end of file ({} lines total)",
            offset.unwrap_or_default(),
            lines.len()
        )));
    }
    let end = limit.map_or(lines.len(), |limit| {
        start.saturating_add(limit as usize).min(lines.len())
    });
    let truncation = truncate_head(
        &lines[start..end].join("\n"),
        DEFAULT_MAX_LINES,
        DEFAULT_MAX_BYTES,
    );
    let first = start + 1;
    let limit_text = format_size(DEFAULT_MAX_BYTES);
    let output = if truncation.first_line_exceeds_limit {
        format!(
            "[Line {first} is {}, exceeds {limit_text} limit. Use bash: sed -n '{first}p' {input_path} | head -c {DEFAULT_MAX_BYTES}]",
            format_size(lines[start].len())
        )
    } else if truncation.truncated {
        let last = start + truncation.output_lines;
        let cause = if truncation.by_lines {
            String::new()
        } else {
            format!(" ({limit_text} limit)")
        };
        format!(
            "{}\n\n[Showing lines {first}-{last} of {}{cause}. Use offset={} to continue.]",
            truncation.content,
            lines.len(),
            last + 1
        )
    } else if limit.is_some() && end < lines.len() {
        format!(
            "{}\n\n[{} more lines in file. Use offset={} to continue.]",
            truncation.content,
            lines.len() - end,
            end + 1
        )
    } else {
        truncation.content
    };
    Ok(ToolResult::text(output))
}

/// The type of an image the model can be sent, from the file's first bytes
/// (upstream `detectSupportedImageMimeType`). Animated PNG and JPEG-LS are
/// not supported and read as text, as upstream does.
fn image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    let bytes = &bytes[..bytes.len().min(4100)];
    let u32_be = |at: usize| {
        bytes
            .get(at..at + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    let u32_le = |at: usize| {
        bytes
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    let u16_le = |at: usize| {
        bytes
            .get(at..at + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    let has = |at: usize, text: &[u8]| bytes.get(at..at + text.len()) == Some(text);
    if has(0, b"\xff\xd8\xff") {
        return (bytes.get(3) != Some(&0xf7)).then_some("image/jpeg");
    }
    if has(0, PNG_SIGNATURE) {
        if bytes.len() < 16 || u32_be(8) != Some(13) || !has(12, b"IHDR") {
            return None;
        }
        // An `acTL` chunk before the image data marks an animated PNG.
        let mut chunk = PNG_SIGNATURE.len();
        while let Some(length) = u32_be(chunk).filter(|_| chunk + 8 <= bytes.len()) {
            if has(chunk + 4, b"acTL") {
                return None;
            }
            if has(chunk + 4, b"IDAT") || chunk + 12 + length > bytes.len() {
                break;
            }
            chunk += 12 + length;
        }
        return Some("image/png");
    }
    if has(0, b"GIF87a") || has(0, b"GIF89a") {
        return Some("image/gif");
    }
    if has(0, b"RIFF") && has(8, b"WEBP") {
        return Some("image/webp");
    }
    if has(0, b"BM") && bytes.len() >= 26 {
        let (file_size, pixel_offset, header_size) = (u32_le(2)?, u32_le(10)?, u32_le(14)?);
        let sizes_valid = (file_size == 0 || (file_size >= 26 && pixel_offset < file_size))
            && pixel_offset >= 14 + header_size;
        let (planes, bits_per_pixel) = match header_size {
            12 => (u16_le(22)?, u16_le(24)?),
            40..=124 => (u16_le(26)?, u16_le(28)?),
            _ => return None,
        };
        let valid = sizes_valid && planes == 1 && [1, 4, 8, 16, 24, 32].contains(&bits_per_pixel);
        return valid.then_some("image/bmp");
    }
    None
}

/// Largest image sent as is; larger ones are scaled down.
const MAX_IMAGE_DIMENSION: u32 = 2000;
/// Limit on the base64 text of an image, below Anthropic's 5 MB.
const MAX_IMAGE_BASE64_BYTES: usize = 4_718_592;

fn read_image(bytes: &[u8], mime_type: &'static str, options: &ToolRuntimeOptions) -> ToolResult {
    if options.block_images {
        return ToolResult::text(format!(
            "Read image file [{mime_type}]\nImage reading is disabled."
        ));
    }
    match process_image(bytes, mime_type, options.auto_resize_images) {
        Ok((image, hints)) => {
            let mut output = format!("Read image file [{}]", image.mime_type);
            for hint in hints {
                output.push('\n');
                output.push_str(&hint);
            }
            ToolResult {
                output,
                is_error: false,
                images: vec![image],
            }
        }
        Err(message) => ToolResult::text(format!("Read image file [{mime_type}]\n{message}")),
    }
}

/// Prepares an image for the model (upstream `processImage`): BMP becomes
/// PNG, and with `auto_resize` an image larger than 2000 pixels on a side or
/// 4.5 MB of base64 is scaled down. Returns the image and notes for the
/// model, or the reason the image is left out.
fn process_image(
    bytes: &[u8],
    mime_type: &'static str,
    auto_resize: bool,
) -> Result<(ToolImage, Vec<String>), String> {
    let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    let converted = mime_type == "image/bmp";
    let (bytes, mime_type) = if converted {
        let png = decode_image(bytes, ImageFormat::Bmp)
            .and_then(|image| encode_png(&image))
            .ok_or("[Image omitted: could not be converted to a supported inline image format.]")?;
        (png, "image/png")
    } else {
        (bytes.to_vec(), mime_type)
    };
    let finish = |data_base64: String, mime_type: &str, resized: Option<String>| {
        let mut hints = Vec::new();
        if converted {
            hints.push(format!("[Image converted from image/bmp to {mime_type}.]"));
        }
        hints.extend(resized);
        let image = ToolImage {
            mime_type: mime_type.to_string(),
            data_base64,
        };
        (image, hints)
    };
    if !auto_resize {
        return Ok(finish(encode(&bytes), mime_type, None));
    }

    let too_large = "[Image omitted: could not be resized below the inline image size limit.]";
    let format = ImageFormat::from_mime_type(mime_type).ok_or(too_large)?;
    let image = decode_image(&bytes, format).ok_or(too_large)?;
    let (original_width, original_height) = (image.width(), image.height());
    if original_width <= MAX_IMAGE_DIMENSION
        && original_height <= MAX_IMAGE_DIMENSION
        && bytes.len().div_ceil(3) * 4 < MAX_IMAGE_BASE64_BYTES
    {
        return Ok(finish(encode(&bytes), mime_type, None));
    }

    let scale = |value: u32, to: u32, from: u32| {
        (f64::from(value) * f64::from(to) / f64::from(from)).round() as u32
    };
    let (mut width, mut height) = (original_width, original_height);
    if width > MAX_IMAGE_DIMENSION {
        height = scale(height, MAX_IMAGE_DIMENSION, width);
        width = MAX_IMAGE_DIMENSION;
    }
    if height > MAX_IMAGE_DIMENSION {
        width = scale(width, MAX_IMAGE_DIMENSION, height);
        height = MAX_IMAGE_DIMENSION;
    }
    let (mut width, mut height) = (width.max(1), height.max(1));
    // PNG first, then JPEG at falling quality; then smaller dimensions.
    loop {
        let resized = image.resize_exact(width, height, image::imageops::FilterType::Lanczos3);
        let candidates = std::iter::once(encode_png(&resized).map(|png| (png, "image/png"))).chain(
            [80, 85, 70, 55, 40]
                .into_iter()
                .map(|quality| encode_jpeg(&resized, quality).map(|jpeg| (jpeg, "image/jpeg"))),
        );
        for (candidate, candidate_type) in candidates.flatten() {
            let data_base64 = encode(&candidate);
            if data_base64.len() < MAX_IMAGE_BASE64_BYTES {
                let note = format!(
                    "[Image: original {original_width}x{original_height}, displayed at {width}x{height}. Multiply coordinates by {:.2} to map to original image.]",
                    f64::from(original_width) / f64::from(width)
                );
                return Ok(finish(data_base64, candidate_type, Some(note)));
            }
        }
        if width == 1 && height == 1 {
            return Err(too_large.to_string());
        }
        width = (width * 3 / 4).max(1);
        height = (height * 3 / 4).max(1);
    }
}

/// Decodes an image and applies its EXIF orientation. Dimensions and memory
/// are bounded so a small file cannot expand without limit.
fn decode_image(bytes: &[u8], format: ImageFormat) -> Option<DynamicImage> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(256 * 1024 * 1024);
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().ok()?;
    let orientation = decoder.orientation().ok()?;
    let mut image = DynamicImage::from_decoder(decoder).ok()?;
    image.apply_orientation(orientation);
    Some(image)
}

fn encode_png(image: &DynamicImage) -> Option<Vec<u8>> {
    let mut output = Cursor::new(Vec::new());
    image.write_to(&mut output, ImageFormat::Png).ok()?;
    Some(output.into_inner())
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut output, quality);
    image.to_rgb8().write_with_encoder(encoder).ok()?;
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        encode_png(&DynamicImage::new_rgb8(width, height)).unwrap()
    }

    #[test]
    fn image_types_are_detected_from_content() {
        assert_eq!(image_mime_type(&png(2, 2)), Some("image/png"));
        assert_eq!(image_mime_type(b"\xff\xd8\xff\xe0rest"), Some("image/jpeg"));
        assert_eq!(image_mime_type(b"\xff\xd8\xff\xf7rest"), None);
        assert_eq!(image_mime_type(b"GIF89a..."), Some("image/gif"));
        assert_eq!(image_mime_type(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(image_mime_type(b"\x89PNG\r\n\x1a\nnot a png"), None);
        assert_eq!(image_mime_type(b"BM, said the text file\n"), None);
        assert_eq!(image_mime_type(b"plain text"), None);

        let mut bmp = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(2, 2)
            .write_to(&mut bmp, ImageFormat::Bmp)
            .unwrap();
        assert_eq!(image_mime_type(bmp.get_ref()), Some("image/bmp"));

        // An animated PNG has an `acTL` chunk before its image data.
        let mut animated = png(1, 1);
        let chunk = b"\0\0\0\x08acTL\0\0\0\x01\0\0\0\0\0\0\0\0";
        animated.splice(33..33, chunk.iter().copied());
        assert_eq!(image_mime_type(&animated), None);
    }

    #[test]
    fn small_images_are_sent_unchanged() {
        let bytes = png(4, 3);
        let (image, hints) = process_image(&bytes, "image/png", true).unwrap();
        assert_eq!(image.mime_type, "image/png");
        assert_eq!(
            image.data_base64,
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        );
        assert!(hints.is_empty());
    }

    #[test]
    fn large_images_are_scaled_to_fit_with_a_coordinate_note() {
        let bytes = png(4000, 1000);
        let (image, hints) = process_image(&bytes, "image/png", true).unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&image.data_base64)
            .unwrap();
        let resized = image::load_from_memory(&decoded).unwrap();
        assert_eq!((resized.width(), resized.height()), (2000, 500));
        assert_eq!(
            hints,
            ["[Image: original 4000x1000, displayed at 2000x500. Multiply coordinates by 2.00 to map to original image.]"]
        );

        // Without auto-resize the image goes out as it is.
        let (image, hints) = process_image(&bytes, "image/png", false).unwrap();
        assert_eq!(
            image.data_base64,
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        );
        assert!(hints.is_empty());
    }

    #[test]
    fn bmp_is_converted_to_png_and_undecodable_images_are_left_out() {
        let mut bmp = Cursor::new(Vec::new());
        DynamicImage::new_rgb8(2, 2)
            .write_to(&mut bmp, ImageFormat::Bmp)
            .unwrap();
        let (image, hints) = process_image(bmp.get_ref(), "image/bmp", true).unwrap();
        assert_eq!(image.mime_type, "image/png");
        assert_eq!(hints, ["[Image converted from image/bmp to image/png.]"]);

        assert_eq!(
            process_image(b"\x89PNG\r\n\x1a\ntruncated", "image/png", true).unwrap_err(),
            "[Image omitted: could not be resized below the inline image size limit.]"
        );
    }
}
