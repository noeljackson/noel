//! Message inputs: @file arguments, images read and resized for a model, and their MIME types.

use super::*;

pub(crate) fn expand_message_inputs(cwd: &Path, messages: &[String]) -> Result<String> {
    let mut parts = Vec::new();
    for message in messages {
        if let Some(path) = message.strip_prefix('@').filter(|path| !path.is_empty()) {
            parts.push(resolve_text_or_file(cwd, path)?);
        } else {
            parts.push(message.clone());
        }
    }
    Ok(parts.join(" "))
}

pub(crate) fn load_media_inputs(
    cwd: &Path,
    paths: &[PathBuf],
    config: &LoadedConfig,
) -> Result<Vec<MediaInput>> {
    if images_blocked(config) && !paths.is_empty() {
        return Err(anyhow!("images are blocked by settings"));
    }
    paths
        .iter()
        .map(|path| load_media_input(cwd, path, config))
        .collect()
}

pub(crate) fn images_auto_resize(config: &LoadedConfig) -> bool {
    config
        .settings
        .images
        .as_ref()
        .and_then(|images| images.auto_resize)
        .unwrap_or(true)
}

pub(crate) fn images_blocked(config: &LoadedConfig) -> bool {
    config
        .settings
        .images
        .as_ref()
        .and_then(|images| images.block_images)
        .unwrap_or(false)
}

pub(crate) fn load_media_input(
    cwd: &Path,
    path: &Path,
    config: &LoadedConfig,
) -> Result<MediaInput> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let bytes = fs::read(&path)?;
    media_input_from_bytes(
        bytes,
        media_mime_type(&path)?,
        Some(path.display().to_string()),
        config,
    )
}

pub(crate) fn media_input_from_bytes(
    mut bytes: Vec<u8>,
    mut mime_type: String,
    path: Option<String>,
    config: &LoadedConfig,
) -> Result<MediaInput> {
    if images_auto_resize(config) {
        if let Some(resized) = resize_image_if_needed(&bytes, &mime_type)? {
            bytes = resized.bytes;
            mime_type = resized.mime_type;
        }
    }
    let (width, height) = image_dimensions(&bytes, &mime_type).unwrap_or((None, None));
    Ok(MediaInput {
        mime_type,
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        path,
        width,
        height,
    })
}

pub(crate) fn clipboard_media(bytes: Vec<u8>, config: &LoadedConfig) -> Result<MediaInput> {
    if images_blocked(config) {
        return Err(anyhow!("images are blocked by settings"));
    }
    let format = image::guess_format(&bytes)?;
    let mime = match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        _ => return Err(anyhow!("unsupported clipboard image format")),
    };
    // Validate dimensions/pixels before attaching arbitrary clipboard bytes.
    let mut reader = image::ImageReader::with_format(Cursor::new(&bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    reader.decode()?;
    media_input_from_bytes(bytes, mime.into(), None, config)
}

pub(crate) async fn paste_clipboard_image(app: &mut TuiApp, config: &LoadedConfig) {
    if images_blocked(config) {
        app.push(TuiEntryKind::Error, "images are blocked by settings");
        return;
    }
    match clipboard::image_bytes()
        .await
        .and_then(|bytes| clipboard_media(bytes, config))
    {
        Ok(media) => app.input_media.push(media),
        Err(error) => app.push(TuiEntryKind::Error, format!("clipboard paste: {error}")),
    }
    app.quit_armed_since = None;
}

pub(crate) const MAX_AUTO_RESIZE_IMAGE_DIMENSION: u32 = 2000;

pub(crate) struct ResizedImage {
    pub(crate) bytes: Vec<u8>,
    pub(crate) mime_type: String,
}

pub(crate) fn resize_image_if_needed(
    bytes: &[u8],
    mime_type: &str,
) -> Result<Option<ResizedImage>> {
    let Some(input_format) = image_format_for_mime_type(mime_type) else {
        return Ok(None);
    };
    let image = match image::load_from_memory_with_format(bytes, input_format) {
        Ok(image) => image,
        Err(_) => return Ok(None),
    };
    if image.width() <= MAX_AUTO_RESIZE_IMAGE_DIMENSION
        && image.height() <= MAX_AUTO_RESIZE_IMAGE_DIMENSION
    {
        return Ok(None);
    }
    let resized = image.thumbnail(
        MAX_AUTO_RESIZE_IMAGE_DIMENSION,
        MAX_AUTO_RESIZE_IMAGE_DIMENSION,
    );
    let output_format = if mime_type == "image/jpeg" {
        image::ImageFormat::Jpeg
    } else {
        image::ImageFormat::Png
    };
    let mut output = Cursor::new(Vec::new());
    resized.write_to(&mut output, output_format)?;
    Ok(Some(ResizedImage {
        bytes: output.into_inner(),
        mime_type: if output_format == image::ImageFormat::Jpeg {
            "image/jpeg".to_string()
        } else {
            "image/png".to_string()
        },
    }))
}

pub(crate) fn image_format_for_mime_type(mime_type: &str) -> Option<image::ImageFormat> {
    match mime_type {
        "image/png" => Some(image::ImageFormat::Png),
        "image/jpeg" => Some(image::ImageFormat::Jpeg),
        "image/gif" => Some(image::ImageFormat::Gif),
        "image/webp" => Some(image::ImageFormat::WebP),
        _ => None,
    }
}

pub(crate) fn media_mime_type(path: &Path) -> Result<String> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_lowercase())
        .as_deref()
    {
        Some("png") => Ok("image/png".to_string()),
        Some("jpg") | Some("jpeg") => Ok("image/jpeg".to_string()),
        Some("gif") => Ok("image/gif".to_string()),
        Some("webp") => Ok("image/webp".to_string()),
        Some(extension) => Err(anyhow!("unsupported image extension: {extension}")),
        None => Err(anyhow!("image path has no extension: {}", path.display())),
    }
}

pub(crate) fn image_dimensions(
    bytes: &[u8],
    mime_type: &str,
) -> Option<(Option<u32>, Option<u32>)> {
    match mime_type {
        "image/png" => png_dimensions(bytes).map(|(width, height)| (Some(width), Some(height))),
        "image/jpeg" => jpeg_dimensions(bytes).map(|(width, height)| (Some(width), Some(height))),
        _ => Some((None, None)),
    }
}

pub(crate) fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[0..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    Some((
        u32::from_be_bytes(bytes[16..20].try_into().ok()?),
        u32::from_be_bytes(bytes[20..24].try_into().ok()?),
    ))
}

pub(crate) fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[0] != 0xff || bytes[1] != 0xd8 {
        return None;
    }
    let mut index = 2;
    while index + 9 < bytes.len() {
        if bytes[index] != 0xff {
            index += 1;
            continue;
        }
        let marker = bytes[index + 1];
        let length = u16::from_be_bytes([bytes[index + 2], bytes[index + 3]]) as usize;
        if matches!(
            marker,
            0xc0 | 0xc1
                | 0xc2
                | 0xc3
                | 0xc5
                | 0xc6
                | 0xc7
                | 0xc9
                | 0xca
                | 0xcb
                | 0xcd
                | 0xce
                | 0xcf
        ) {
            let height = u16::from_be_bytes([bytes[index + 5], bytes[index + 6]]) as u32;
            let width = u16::from_be_bytes([bytes[index + 7], bytes[index + 8]]) as u32;
            return Some((width, height));
        }
        if length < 2 {
            return None;
        }
        index += length + 2;
    }
    None
}
