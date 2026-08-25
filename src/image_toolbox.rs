use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{GenericImageView, ImageFormat};
use reqwest::{Url, blocking::Client};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    event::image_content_sha256,
    toolbox::{ToolboxExecutionError, ToolboxTool, api_safe_name},
};

pub const TOOLBOX_NAME: &str = "Image";
pub const INFO_TOOL_NAME: &str = "Image.Info";
pub const VIEW_TOOL_NAME: &str = "Image.View";
pub const WEB_BROWSER_SNAPSHOT_TOOL_NAME: &str = "WebBrowser.Snapshot";
const MAX_IMAGE_BYTES: usize = 60 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
pub struct ImageMetadata {
    pub source: String,
    pub format: String,
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub aspect_ratio: f64,
    pub color_type: String,
    pub bits_per_pixel: u16,
    pub has_alpha: bool,
    pub bytes: usize,
    pub sha256: String,
}

#[derive(Clone, Debug)]
pub struct LoadedImage {
    pub metadata: ImageMetadata,
    pub data: Vec<u8>,
}

pub fn model_supports_images(model: &crate::config::ModelConfig) -> bool {
    model
        .capabilities
        .input_modalities
        .iter()
        .any(|modality| modality.eq_ignore_ascii_case("image"))
}

pub fn stores_image_content(tool_name: &str) -> bool {
    matches!(tool_name, VIEW_TOOL_NAME | WEB_BROWSER_SNAPSHOT_TOOL_NAME)
}

pub fn tool_call_requires_image_content(tool_name: &str, _arguments: &str) -> bool {
    tool_name == VIEW_TOOL_NAME
}

pub fn catalog_parts(image_input_supported: bool) -> (Vec<ToolboxTool>, (String, String)) {
    let info_route = "Inspect an image's encoded format, dimensions, color layout, byte size, and content hash without adding the image to model context.";
    let view_route = if image_input_supported {
        "Load an image into the conversation so you can inspect its visual content. Use Image.Info when metadata alone is sufficient."
    } else {
        "The current model does not support image input. Image.View will reject calls until an image-capable model is selected; Image.Info remains available."
    };
    let view_instructions = if image_input_supported {
        "Loads the complete image binary from the supplied URL or local path, validates and identifies it, and stores the complete image with the conversation before exposing it to the model. The conversation remains replayable even if the original file or URL later disappears."
    } else {
        "Unavailable with the current model because it does not accept image input. Select an image-capable model before calling this tool. The restriction is enforced by the runtime."
    };
    let brief = if image_input_supported {
        "Inspect image metadata and view visual image content. Image.View is available because the current model supports image input. Viewed images are stored as complete binary content in the conversation, not as fragile external references."
    } else {
        "Inspect image metadata. The current model does not support image input, so Image.View is unavailable and will be rejected until an image-capable model is selected."
    };
    let schema = json!({
        "type": "object",
        "required": ["url"],
        "properties": {
            "url": {
                "type": "string",
                "minLength": 1,
                "description": "Non-empty HTTP(S) URL, file URL, data URL, or local image path. Relative paths resolve from the workspace; whitespace-only values are invalid."
            }
        },
        "additionalProperties": false
    });
    let metadata_schema = json!({
        "type": "object",
        "required": ["source", "format", "mime_type", "width", "height", "aspect_ratio", "color_type", "bits_per_pixel", "has_alpha", "bytes", "sha256"],
        "properties": {
            "source": {"type": "string"},
            "format": {"type": "string"},
            "mime_type": {"type": "string"},
            "width": {"type": "integer"},
            "height": {"type": "integer"},
            "aspect_ratio": {"type": "number"},
            "color_type": {"type": "string"},
            "bits_per_pixel": {"type": "integer"},
            "has_alpha": {"type": "boolean"},
            "bytes": {"type": "integer"},
            "sha256": {"type": "string"}
        },
        "additionalProperties": false
    });
    let info = ToolboxTool {
        toolbox: TOOLBOX_NAME.into(),
        local_name: "Info".into(),
        full_name: INFO_TOOL_NAME.into(),
        api_name: api_safe_name(INFO_TOOL_NAME),
        input_schema: schema.clone(),
        output_schema: metadata_schema.clone(),
        instructions: "Reads and decodes the image only far enough to return reliable metadata. It does not add image content to model context.".into(),
        route: info_route.into(),
        examples: r#"{"url":"./diagram.png"}
{"url":"https://example.com/photo.webp"}"#.into(),
    };
    let view = ToolboxTool {
        toolbox: TOOLBOX_NAME.into(),
        local_name: "View".into(),
        full_name: VIEW_TOOL_NAME.into(),
        api_name: api_safe_name(VIEW_TOOL_NAME),
        input_schema: schema,
        output_schema: json!({
            "type": "object",
            "required": ["image_event_id", "image"],
            "properties": {
                "image_event_id": {"type": "integer"},
                "image": metadata_schema
            },
            "additionalProperties": false
        }),
        instructions: view_instructions.into(),
        route: view_route.into(),
        examples: r#"{"url":"./web-snapshot-a1b2c3d.png"}
{"url":"https://example.com/chart.jpg"}"#
            .into(),
    };
    (vec![info, view], (TOOLBOX_NAME.into(), brief.into()))
}

pub fn load(arguments: &str, workspace: &Path) -> Result<LoadedImage, ToolboxExecutionError> {
    let input: Value = serde_json::from_str(arguments)
        .map_err(|error| tool_error("invalid_arguments", error.to_string(), false))?;
    let url = input
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| !url.trim().is_empty())
        .ok_or_else(|| tool_error("invalid_arguments", "url must be a non-empty string", false))?;
    let data = read_source(url, workspace)?;
    let format = image::guess_format(&data)
        .map_err(|error| tool_error("unsupported_image", error.to_string(), false))?;
    let image = image::load_from_memory_with_format(&data, format)
        .map_err(|error| tool_error("invalid_image", error.to_string(), false))?;
    let (width, height) = image.dimensions();
    let color = image.color();
    let metadata = ImageMetadata {
        source: normalized_source(url, workspace),
        format: format_name(format).into(),
        mime_type: format.to_mime_type().into(),
        width,
        height,
        aspect_ratio: f64::from(width) / f64::from(height),
        color_type: format!("{color:?}"),
        bits_per_pixel: color.bits_per_pixel(),
        has_alpha: color.has_alpha(),
        bytes: data.len(),
        sha256: image_content_sha256(&data),
    };
    Ok(LoadedImage { metadata, data })
}

pub fn metadata_value(image: &LoadedImage) -> Value {
    serde_json::to_value(&image.metadata).expect("ImageMetadata serialization cannot fail")
}

pub fn model_context_png(data: &[u8]) -> std::result::Result<Vec<u8>, image::ImageError> {
    let image = image::load_from_memory(data)?;
    let mut encoded = std::io::Cursor::new(Vec::new());
    image.write_to(&mut encoded, ImageFormat::Png)?;
    Ok(encoded.into_inner())
}

fn read_source(source: &str, workspace: &Path) -> Result<Vec<u8>, ToolboxExecutionError> {
    if source.starts_with("data:") {
        return decode_data_url(source);
    }
    if source.starts_with("http://") || source.starts_with("https://") {
        let client = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|error| tool_error("image_request_failed", error.to_string(), true))?;
        let response = client
            .get(source)
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(|error| tool_error("image_request_failed", error.to_string(), true))?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_IMAGE_BYTES as u64)
        {
            return Err(tool_error(
                "image_too_large",
                format!("image exceeds the {MAX_IMAGE_BYTES} byte limit"),
                false,
            ));
        }
        return read_limited(response, source);
    }
    let path = if source.starts_with("file:") {
        Url::parse(source)
            .ok()
            .and_then(|url| url.to_file_path().ok())
            .ok_or_else(|| tool_error("invalid_arguments", "invalid file URL", false))?
    } else {
        let path = PathBuf::from(source);
        if path.is_absolute() {
            path
        } else {
            workspace.join(path)
        }
    };
    let metadata = fs::metadata(&path)
        .map_err(|error| tool_error("image_read_failed", error.to_string(), false))?;
    if metadata.len() > MAX_IMAGE_BYTES as u64 {
        return Err(tool_error(
            "image_too_large",
            format!("image exceeds the {MAX_IMAGE_BYTES} byte limit"),
            false,
        ));
    }
    fs::read(&path).map_err(|error| tool_error("image_read_failed", error.to_string(), false))
}

fn read_limited(
    response: reqwest::blocking::Response,
    source: &str,
) -> Result<Vec<u8>, ToolboxExecutionError> {
    let mut data = Vec::new();
    response
        .take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|error| tool_error("image_request_failed", format!("{source}: {error}"), true))?;
    if data.len() > MAX_IMAGE_BYTES {
        return Err(tool_error(
            "image_too_large",
            format!("image exceeds the {MAX_IMAGE_BYTES} byte limit"),
            false,
        ));
    }
    Ok(data)
}

fn decode_data_url(source: &str) -> Result<Vec<u8>, ToolboxExecutionError> {
    let (header, payload) = source
        .split_once(',')
        .ok_or_else(|| tool_error("invalid_arguments", "invalid data URL", false))?;
    if !header.ends_with(";base64") {
        return Err(tool_error(
            "invalid_arguments",
            "only base64 image data URLs are supported",
            false,
        ));
    }
    let data = STANDARD
        .decode(payload)
        .map_err(|error| tool_error("invalid_arguments", error.to_string(), false))?;
    if data.len() > MAX_IMAGE_BYTES {
        return Err(tool_error(
            "image_too_large",
            format!("image exceeds the {MAX_IMAGE_BYTES} byte limit"),
            false,
        ));
    }
    Ok(data)
}

fn normalized_source(source: &str, workspace: &Path) -> String {
    if source.starts_with("data:") {
        return "data:image".into();
    }
    if source.contains("://") {
        return source.into();
    }
    let path = Path::new(source);
    if path.is_absolute() {
        return crate::host_path::public_host_path(path);
    }
    crate::host_path::public_host_path(workspace.join(path))
}

fn format_name(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpeg",
        ImageFormat::Gif => "gif",
        ImageFormat::WebP => "webp",
        ImageFormat::Pnm => "pnm",
        ImageFormat::Tiff => "tiff",
        ImageFormat::Tga => "tga",
        ImageFormat::Dds => "dds",
        ImageFormat::Bmp => "bmp",
        ImageFormat::Ico => "ico",
        ImageFormat::Hdr => "hdr",
        ImageFormat::OpenExr => "openexr",
        ImageFormat::Farbfeld => "farbfeld",
        ImageFormat::Avif => "avif",
        ImageFormat::Qoi => "qoi",
        _ => "unknown",
    }
}

fn tool_error(
    code: impl Into<String>,
    message: impl Into<String>,
    retryable: bool,
) -> ToolboxExecutionError {
    ToolboxExecutionError::Tool {
        code: code.into(),
        message: message.into(),
        retryable,
        tip: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};
    use std::{
        io::{BufRead, BufReader, Cursor, Write},
        net::TcpListener,
        thread,
    };

    fn sample_png() -> Vec<u8> {
        let image = image::DynamicImage::ImageRgba8(ImageBuffer::from_pixel(
            3,
            2,
            Rgba([10_u8, 20, 30, 255]),
        ));
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Png).unwrap();
        bytes.into_inner()
    }

    #[test]
    fn loads_metadata_from_a_local_png() {
        let root = std::env::temp_dir().join(format!("me-image-toolbox-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("sample.png");
        fs::write(&path, sample_png()).unwrap();
        let loaded = load(r#"{"url":"sample.png"}"#, &root).unwrap();
        assert_eq!((loaded.metadata.width, loaded.metadata.height), (3, 2));
        assert_eq!(loaded.metadata.format, "png");
        assert_eq!(loaded.metadata.mime_type, "image/png");
        assert_eq!(loaded.metadata.sha256, image_content_sha256(&loaded.data));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn model_context_projection_decodes_source_formats_and_always_encodes_png() {
        let image = image::DynamicImage::ImageRgb8(ImageBuffer::from_pixel(
            3,
            2,
            image::Rgb([10_u8, 20, 30]),
        ));
        for source_format in [
            ImageFormat::Png,
            ImageFormat::Jpeg,
            ImageFormat::Gif,
            ImageFormat::WebP,
            ImageFormat::Bmp,
            ImageFormat::Tiff,
            ImageFormat::Pnm,
        ] {
            let mut source = Cursor::new(Vec::new());
            image.write_to(&mut source, source_format).unwrap();
            let source = source.into_inner();

            let projected = model_context_png(&source).unwrap();
            assert_eq!(image::guess_format(&projected).unwrap(), ImageFormat::Png);
            assert_eq!(
                image::load_from_memory(&projected).unwrap().dimensions(),
                (3, 2)
            );
        }
    }

    #[test]
    fn loads_data_file_and_http_urls_and_rejects_oversized_sources() {
        let root =
            std::env::temp_dir().join(format!("me-image-toolbox-protocols-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let bytes = sample_png();
        let path = root.join("sample.png");
        fs::write(&path, &bytes).unwrap();

        let data_url = format!("data:image/png;base64,{}", STANDARD.encode(&bytes));
        assert_eq!(
            load(
                &serde_json::to_string(&json!({"url": data_url})).unwrap(),
                &root
            )
            .unwrap()
            .data,
            bytes
        );
        let file_url = Url::from_file_path(&path).unwrap().to_string();
        assert_eq!(
            load(
                &serde_json::to_string(&json!({"url": file_url})).unwrap(),
                &root
            )
            .unwrap()
            .metadata
            .width,
            3
        );

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let response_bytes = bytes.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_bytes.len()
            )
            .unwrap();
            stream.write_all(&response_bytes).unwrap();
        });
        let loaded = load(
            &serde_json::to_string(&json!({"url": format!("http://{address}/sample.png")}))
                .unwrap(),
            &root,
        )
        .unwrap();
        assert_eq!((loaded.metadata.width, loaded.metadata.height), (3, 2));
        server.join().unwrap();

        let oversized = root.join("oversized.png");
        let file = fs::File::create(&oversized).unwrap();
        file.set_len(MAX_IMAGE_BYTES as u64 + 1).unwrap();
        let error = load(r#"{"url":"oversized.png"}"#, &root).unwrap_err();
        assert!(error.to_string().contains("image_too_large"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dynamic_catalog_describes_current_model_capability() {
        let (supported, brief) = catalog_parts(true);
        assert!(brief.1.contains("current model supports image input"));
        assert!(supported[1].route.contains("visual content"));
        assert_eq!(
            supported[0].input_schema["properties"]["url"]["minLength"],
            1
        );
        assert!(
            supported[0].input_schema["properties"]["url"]["description"]
                .as_str()
                .unwrap()
                .contains("whitespace-only")
        );
        assert!(!supported[1].instructions.contains("event database"));
        let (unsupported, brief) = catalog_parts(false);
        assert!(brief.1.contains("does not support image input"));
        assert!(unsupported[1].route.contains("will reject"));
    }

    #[test]
    fn only_image_view_requires_new_image_content() {
        assert!(tool_call_requires_image_content(
            VIEW_TOOL_NAME,
            r#"{"url":"a.png"}"#
        ));
        assert!(!tool_call_requires_image_content(
            WEB_BROWSER_SNAPSHOT_TOOL_NAME,
            r#"{"page_id":"p0000001","wait_ms":1000,"kind":"screen"}"#,
        ));
        assert!(stores_image_content(WEB_BROWSER_SNAPSHOT_TOOL_NAME));
    }
}
