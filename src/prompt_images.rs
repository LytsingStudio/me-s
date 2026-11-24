use crate::{
    Result,
    event::{ImageContentEvent, PromptImage},
    workspace::AgentId,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use image::ImageDecoder;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Cursor,
    path::{Path, PathBuf},
    sync::Arc,
};

pub(crate) const MAX_IMAGES: usize = 16;
pub(crate) const MAX_BYTES: u64 = 60 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OriginalImage {
    pub name: String,
    #[serde(default)]
    pub mime_type: String,
    pub last_modified_ms: Option<u64>,
}

pub(crate) fn directory(workspace: &Path, agent: &AgentId) -> PathBuf {
    workspace
        .join(".me/tmp")
        .join(agent.as_str())
        .join("paste-images")
}

pub(crate) fn reserve(directory: &Path, original: &OriginalImage, size: u64) -> Result<PathBuf> {
    if size == 0 || size > MAX_BYTES {
        return Err("图片大小须在 1 字节至 60 MiB 之间".into());
    }
    let extension = Path::new(&original.name)
        .extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
        .or_else(|| match original.mime_type.as_str() {
            "image/png" => Some("png"),
            "image/jpeg" => Some("jpg"),
            "image/tiff" => Some("tif"),
            "image/webp" => Some("webp"),
            "image/gif" => Some("gif"),
            _ => None,
        })
        .ok_or("无法识别图片格式")?;
    if !matches!(
        extension.to_ascii_lowercase().as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "tif"
            | "tiff"
            | "gif"
            | "webp"
            | "bmp"
            | "ico"
            | "avif"
            | "qoi"
            | "pnm"
            | "pbm"
            | "pgm"
            | "ppm"
            | "tga"
            | "dds"
            | "hdr"
            | "exr"
    ) {
        return Err("不支持此图片格式".into());
    }
    fs::create_dir_all(directory)?;
    loop {
        let mut random = [0u8; 4];
        getrandom::fill(&mut random)?;
        let path = directory.join(format!(
            "img-{:08x}.{extension}",
            u32::from_ne_bytes(random)
        ));
        if path.exists() {
            continue;
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(path.with_extension("yaml")) {
            Ok(_) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn load(path: &Path) -> Result<crate::image_toolbox::LoadedImage> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err("图片文件不可用".into());
    }
    let loaded = crate::image_toolbox::load_view(
        &json!({"url":crate::host_path::public_host_path(path)}).to_string(),
        Path::new("."),
    )?;
    crate::image_toolbox::validate_info_dimensions(&loaded.metadata)?;
    Ok(loaded)
}

fn descriptor(path: &Path, loaded: &crate::image_toolbox::LoadedImage) -> PromptImage {
    PromptImage {
        path: crate::host_path::public_host_path(path),
        sha256: loaded.metadata.sha256.clone(),
        format: loaded.metadata.format.clone(),
        width: loaded.metadata.width,
        height: loaded.metadata.height,
    }
}

pub(crate) fn finish(path: &Path, original: &OriginalImage) -> Result<PromptImage> {
    let loaded = load(path)?;
    let image = descriptor(path, &loaded);
    let exif = image::ImageReader::new(Cursor::new(&loaded.data))
        .with_guessed_format()?
        .into_decoder()?
        .exif_metadata()
        .ok()
        .flatten();
    let mut metadata = json!({
        "original_name":original.name, "original_mime_type":original.mime_type,
        "last_modified_ms":original.last_modified_ms, "path":image.path,
        "format":loaded.metadata.format, "mime_type":loaded.metadata.mime_type,
        "width":image.width, "height":image.height, "bytes":loaded.data.len(), "sha256":image.sha256,
    });
    if let Some(exif) = exif {
        metadata["exif"] = exif_metadata(&exif);
        metadata["original_exif_base64"] = json!(BASE64.encode(exif));
    }
    let mut yaml = String::new();
    for (key, value) in metadata.as_object().unwrap() {
        yaml.push_str(&format!("{key}: {}\n", serde_json::to_string(value)?));
    }
    crate::config::write_private_atomic(&path.with_extension("yaml"), yaml.as_bytes())?;
    Ok(image)
}

pub(crate) fn resolve(
    workspace: &Path,
    agent: &AgentId,
    paths: Vec<String>,
) -> Result<Vec<PromptImage>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    if paths.len() > MAX_IMAGES {
        return Err("每条消息最多添加 16 张图片".into());
    }
    let allowed = fs::canonicalize(directory(workspace, agent))?;
    let mut total = 0;
    paths
        .into_iter()
        .map(|path| {
            let path = PathBuf::from(path);
            let filename = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or("图片文件名无效")?;
            let stem = Path::new(filename)
                .file_stem()
                .and_then(|name| name.to_str())
                .ok_or("图片文件名无效")?;
            if stem.len() != 12
                || !stem.starts_with("img-")
                || !stem[4..]
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            {
                return Err("图片文件名无效".into());
            }
            if fs::canonicalize(path.parent().ok_or("图片路径无效")?)? != allowed
                || !path.with_extension("yaml").is_file()
            {
                return Err("图片不属于当前会话".into());
            }
            let loaded = load(&path)?;
            total += loaded.data.len() as u64;
            if total > MAX_BYTES {
                return Err("每条消息的图片总大小不能超过 60 MiB".into());
            }
            Ok(descriptor(&path, &loaded))
        })
        .collect()
}

pub(crate) fn content(
    image: &PromptImage,
    id: u64,
    timestamp_ms: u64,
) -> Result<ImageContentEvent> {
    let loaded = load(Path::new(&image.path))?;
    if loaded.metadata.sha256 != image.sha256 {
        return Err("图片文件已发生变化".into());
    }
    // This is a read-only UI value, never an event appended to the EDB or model context.
    Ok(ImageContentEvent {
        id,
        timestamp_ms,
        tool_call_id: 0,
        source: image.path.clone(),
        mime_type: loaded.metadata.mime_type,
        format: loaded.metadata.format,
        width: image.width,
        height: image.height,
        content_sha256: image.sha256.clone(),
        data: Arc::from(loaded.data),
    })
}

pub(crate) fn notification(images: &[PromptImage]) -> String {
    if images.is_empty() {
        return String::new();
    }
    let paths: Vec<_> = images.iter().map(|image| &image.path).collect();
    format!(
        "This user message includes {} uploaded image attachment(s), in the order below. Only local file paths are provided; the images have not been viewed or added to your image context. Use Image.View on a path when you need to inspect its image, if the current model supports image input.\n{}",
        images.len(),
        serde_json::to_string_pretty(&paths).expect("paths serialize")
    )
}

// EXIF is optional external data: retain its original bytes and extract common fields and GPS without trusting offsets.
fn exif_metadata(bytes: &[u8]) -> Value {
    let data = bytes.strip_prefix(b"Exif\0\0").unwrap_or(bytes);
    let little = match data.get(..2) {
        Some(b"II") => true,
        Some(b"MM") => false,
        _ => return json!({}),
    };
    let u16_at = |offset: usize| -> Option<u16> {
        let b: [u8; 2] = data.get(offset..offset.checked_add(2)?)?.try_into().ok()?;
        Some(if little {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    };
    let u32_at = |offset: usize| -> Option<u32> {
        let b: [u8; 4] = data.get(offset..offset.checked_add(4)?)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    };
    if u16_at(2) != Some(42) {
        return json!({});
    }
    let fields = |offset: usize| -> std::collections::BTreeMap<u16, (u16, &[u8])> {
        let mut result = std::collections::BTreeMap::new();
        let count = usize::from(u16_at(offset).unwrap_or(0)).min(4096);
        for index in 0..count {
            let Some(entry) = offset
                .checked_add(2)
                .and_then(|start| start.checked_add(index * 12))
            else {
                break;
            };
            let (Some(tag), Some(kind), Some(count)) =
                (u16_at(entry), u16_at(entry + 2), u32_at(entry + 4))
            else {
                break;
            };
            let unit = match kind {
                1 | 2 | 7 => 1,
                3 => 2,
                4 | 9 => 4,
                5 | 10 => 8,
                _ => continue,
            };
            let Some(size) = (count as usize).checked_mul(unit) else {
                continue;
            };
            let Some(start) = (if size <= 4 {
                Some(entry + 8)
            } else {
                u32_at(entry + 8).map(|value| value as usize)
            }) else {
                continue;
            };
            if let Some(value) = start.checked_add(size).and_then(|end| data.get(start..end)) {
                result.insert(tag, (kind, value));
            }
        }
        result
    };
    let integer = |value: &(u16, &[u8])| -> Option<u32> {
        match value.0 {
            3 => {
                let b = value.1.get(..2)?.try_into().ok()?;
                Some(u32::from(if little {
                    u16::from_le_bytes(b)
                } else {
                    u16::from_be_bytes(b)
                }))
            }
            4 => {
                let b = value.1.get(..4)?.try_into().ok()?;
                Some(if little {
                    u32::from_le_bytes(b)
                } else {
                    u32::from_be_bytes(b)
                })
            }
            _ => None,
        }
    };
    let ascii = |value: &(u16, &[u8])| -> Option<String> {
        (value.0 == 2).then(|| {
            String::from_utf8_lossy(value.1)
                .trim_end_matches('\0')
                .to_string()
        })
    };
    let rational = |value: &(u16, &[u8])| -> Option<Vec<f64>> {
        if value.0 != 5 {
            return None;
        }
        value
            .1
            .chunks_exact(8)
            .map(|chunk| {
                let a = chunk[..4].try_into().ok()?;
                let b = chunk[4..].try_into().ok()?;
                let numerator = if little {
                    u32::from_le_bytes(a)
                } else {
                    u32::from_be_bytes(a)
                };
                let denominator = if little {
                    u32::from_le_bytes(b)
                } else {
                    u32::from_be_bytes(b)
                };
                (denominator != 0).then(|| f64::from(numerator) / f64::from(denominator))
            })
            .collect()
    };
    let root = fields(u32_at(4).unwrap_or(0) as usize);
    let mut result = serde_json::Map::new();
    for (tag, name) in [
        (0x010f, "camera_make"),
        (0x0110, "camera_model"),
        (0x0132, "modified_at"),
        (0x0131, "software"),
        (0x013b, "artist"),
        (0x8298, "copyright"),
    ] {
        if let Some(value) = root.get(&tag).and_then(ascii) {
            result.insert(name.into(), json!(value));
        }
    }
    if let Some(value) = root.get(&0x0112).and_then(integer) {
        result.insert("orientation".into(), json!(value));
    }
    if let Some(offset) = root.get(&0x8769).and_then(integer) {
        let extra = fields(offset as usize);
        for (tag, name) in [
            (0x9003, "captured_at"),
            (0x9004, "digitized_at"),
            (0xa434, "lens_model"),
        ] {
            if let Some(value) = extra.get(&tag).and_then(ascii) {
                result.insert(name.into(), json!(value));
            }
        }
    }
    if let Some(offset) = root.get(&0x8825).and_then(integer) {
        let gps = fields(offset as usize);
        let coordinate = |tag, reference| -> Option<f64> {
            let values = gps.get(&tag).and_then(rational)?;
            if values.len() != 3 {
                return None;
            }
            if values[1] >= 60.0 || values[2] >= 60.0 {
                return None;
            }
            let value = values[0] + values[1] / 60.0 + values[2] / 3600.0;
            let hemisphere = gps.get(&reference).and_then(ascii)?;
            let limit = if tag == 2 { 90.0 } else { 180.0 };
            if value > limit {
                return None;
            }
            match (tag, hemisphere.as_str()) {
                (2, "N") | (4, "E") => Some(value),
                (2, "S") | (4, "W") => Some(-value),
                _ => None,
            }
        };
        if let (Some(latitude), Some(longitude)) = (coordinate(2, 1), coordinate(4, 3)) {
            result.insert(
                "location".into(),
                json!({"latitude":latitude,"longitude":longitude}),
            );
        }
    }
    Value::Object(result)
}

pub(crate) fn clone_attachments(
    events: &mut [crate::event::Event],
    edb_path: &Path,
) -> Result<Vec<PathBuf>> {
    use crate::event::Event;
    let mut created = Vec::new();
    let result = (|| -> Result<()> {
        let mut copies = std::collections::BTreeMap::<String, PromptImage>::new();
        for event in events {
            let images = match event {
                Event::UserPrompt(prompt) => &mut prompt.images,
                Event::FollowUpPrompt(prompt) => &mut prompt.images,
                _ => continue,
            };
            for image in images {
                if let Some(copy) = copies.get(&image.path) {
                    *image = copy.clone();
                    continue;
                }
                let source = Path::new(&image.path);
                let loaded = load(source)?;
                if loaded.metadata.sha256 != image.sha256 {
                    return Err("图片文件已发生变化".into());
                }
                let original = OriginalImage {
                    name: source
                        .file_name()
                        .ok_or("Invalid image path")?
                        .to_string_lossy()
                        .into_owned(),
                    mime_type: loaded.metadata.mime_type,
                    last_modified_ms: None,
                };
                let directory = edb_path
                    .parent()
                    .and_then(Path::parent)
                    .ok_or("Invalid cloned session path")?
                    .join("tmp")
                    .join(edb_path.file_stem().ok_or("Invalid session name")?)
                    .join("paste-images");
                let target = reserve(&directory, &original, loaded.data.len() as u64)?;
                created.push(target.with_extension("yaml"));
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)?;
                created.push(target.clone());
                std::io::Write::write_all(&mut output, &loaded.data)?;
                output.sync_all()?;
                let mut copy = image.clone();
                copy.path = crate::host_path::public_host_path(&target);
                let sidecar = fs::read_to_string(source.with_extension("yaml"))?;
                let yaml = sidecar
                    .lines()
                    .map(|line| {
                        if line.starts_with("path: ") {
                            format!(
                                "path: {}",
                                serde_json::to_string(&copy.path).expect("path serializes")
                            )
                        } else {
                            line.to_owned()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n";
                crate::config::write_private_atomic(
                    &target.with_extension("yaml"),
                    yaml.as_bytes(),
                )?;
                copies.insert(image.path.clone(), copy.clone());
                *image = copy;
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        for path in &created {
            let _ = fs::remove_file(path);
        }
        return Err(error);
    }
    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        event::{Event, EventBase, EventDataBase},
        host_files::HostFileManager,
    };

    fn fixture() -> PathBuf {
        let mut suffix = [0u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        let root = std::env::temp_dir().join(format!(
            "me-prompt-images-{:016x}",
            u64::from_ne_bytes(suffix)
        ));
        fs::create_dir_all(root.join(".me/edb")).unwrap();
        root
    }
    fn original() -> OriginalImage {
        OriginalImage {
            name: "原图.PNG".into(),
            mime_type: "image/png".into(),
            last_modified_ms: Some(123456),
        }
    }
    fn png() -> Vec<u8> {
        let mut data = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(3, 2)
            .write_to(&mut data, image::ImageFormat::Png)
            .unwrap();
        data.into_inner()
    }
    fn upload(manager: &HostFileManager, agent: &AgentId, data: &[u8]) -> PromptImage {
        let created = manager
            .create_paste_image_upload(agent, original(), data.len() as u64)
            .unwrap()
            .upload
            .unwrap();
        manager
            .upload_chunk(&created.upload_id, 0, &BASE64.encode(data))
            .unwrap();
        manager
            .finish_upload(&created.upload_id)
            .unwrap()
            .image
            .unwrap()
    }

    #[test]
    fn paste_images_preserve_bytes_metadata_order_and_agent_boundary() {
        let root = fixture();
        let manager = HostFileManager::new(&root).unwrap();
        let agent = AgentId::new("agent-12345678").unwrap();
        let bytes = png();
        let first = upload(&manager, &agent, &bytes);
        let second = upload(&manager, &agent, &bytes);
        assert_ne!(first.path, second.path);
        assert_eq!(Path::new(&first.path).extension().unwrap(), "PNG");
        assert_eq!(Path::new(&first.path).file_stem().unwrap().len(), 12);
        assert_eq!(fs::read(&first.path).unwrap(), bytes);
        let yaml = fs::read_to_string(Path::new(&first.path).with_extension("yaml")).unwrap();
        assert!(yaml.contains("original_name: \"原图.PNG\""));
        assert!(yaml.contains("last_modified_ms: 123456"));
        assert!(yaml.contains("width: 3\n"));
        assert!(!yaml.contains("location"));
        let paths = vec![second.path.clone(), first.path.clone()];
        assert_eq!(
            resolve(&root, &agent, paths.clone()).unwrap(),
            vec![second, first.clone()]
        );
        assert!(resolve(&root, &AgentId::new("agent-87654321").unwrap(), paths).is_err());
        fs::write(&first.path, b"changed").unwrap();
        assert!(content(&first, 1, 1).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn paste_images_cancel_incomplete_and_invalid_uploads_clean_only_owned_files() {
        let root = fixture();
        let manager = HostFileManager::new(&root).unwrap();
        let agent = AgentId::new("agent-12345678").unwrap();
        let dir = directory(&root, &agent);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("unrelated.txt"), b"keep").unwrap();
        for mode in ["cancel", "incomplete", "invalid"] {
            let created = manager
                .create_paste_image_upload(&agent, original(), 4)
                .unwrap()
                .upload
                .unwrap();
            let target = Path::new(&created.target_path);
            if mode == "cancel" {
                manager.cancel_upload(&created.upload_id).unwrap();
            } else {
                if mode == "invalid" {
                    manager
                        .upload_chunk(&created.upload_id, 0, &BASE64.encode(b"nope"))
                        .unwrap();
                }
                assert!(manager.finish_upload(&created.upload_id).is_err());
            }
            assert!(!target.exists());
            assert!(!target.with_extension("yaml").exists());
        }
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        assert_eq!(fs::read(dir.join("unrelated.txt")).unwrap(), b"keep");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn paste_images_edb_roundtrip_hash_migration_and_clone_are_independent() {
        let root = fixture();
        let manager = HostFileManager::new(&root).unwrap();
        let agent = AgentId::new("agent-12345678").unwrap();
        let image = upload(&manager, &agent, &png());
        let path = root.join(".me/edb/agent-12345678.edb");
        let mut edb = EventDataBase::open(&path).unwrap();
        edb.append_agent_kind_def(crate::event::AgentKind::Interactive, "chatbot", None, None)
            .unwrap();
        let prompt = edb
            .append_user_prompt_with_images("<inspect>", vec![image.clone()])
            .unwrap();
        edb.append_agent_turn(prompt, prompt, crate::event::AgentTurnState::Started, "")
            .unwrap();
        edb.append_follow_up_prompt_with_images(prompt, "follow", vec![image.clone()])
            .unwrap();
        let end = edb
            .append_agent_turn(prompt, prompt, crate::event::AgentTurnState::Completed, "")
            .unwrap();
        assert!(
            !edb.events()
                .iter()
                .any(|event| matches!(event, Event::ImageContent(_)))
        );
        let hash = edb.get(prompt).unwrap().getHash();
        let Event::UserPrompt(mut changed) = edb.get(prompt).unwrap().clone() else {
            unreachable!()
        };
        changed.images.clear();
        assert_ne!(changed.getHash(), hash);
        let reopened = EventDataBase::open(&path).unwrap();
        assert_eq!(reopened.get(prompt).unwrap().getHash(), hash);
        drop(reopened);
        let clone_path = root.join(".me/edb/agent-87654321.edb");
        let cloned = edb
            .clone_through_final_answer(end, &clone_path, "copy")
            .unwrap();
        let Event::UserPrompt(copy) = cloned.get(prompt).unwrap() else {
            unreachable!()
        };
        assert_ne!(copy.images[0].path, image.path);
        fs::remove_dir_all(directory(&root, &agent)).unwrap();
        assert_eq!(content(&copy.images[0], 1, 1).unwrap().data.as_ref(), png());
        drop(cloned);
        drop(edb);
        let legacy_path = root.join(".me/edb/legacy.edb");
        let mut legacy = EventDataBase::open(&legacy_path).unwrap();
        let id = legacy.append_user_prompt("legacy").unwrap();
        let old_hash = legacy.get(id).unwrap().getHash();
        drop(legacy);
        let mut bytes = fs::read(&legacy_path).unwrap();
        bytes[4] = 41;
        fs::write(&legacy_path, bytes).unwrap();
        let migrated = EventDataBase::open(&legacy_path).unwrap();
        assert_eq!(migrated.get(id).unwrap().getHash(), old_hash);
        assert_eq!(fs::read(&legacy_path).unwrap()[4], 42);
        drop(migrated);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn paste_images_exif_gps_requires_real_hemispheres_and_valid_rationals() {
        let mut bytes = vec![0u8; 128];
        bytes[..8].copy_from_slice(b"II*\0\x08\0\0\0");
        bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
        fn entry(bytes: &mut [u8], at: usize, tag: u16, kind: u16, count: u32, value: u32) {
            bytes[at..at + 2].copy_from_slice(&tag.to_le_bytes());
            bytes[at + 2..at + 4].copy_from_slice(&kind.to_le_bytes());
            bytes[at + 4..at + 8].copy_from_slice(&count.to_le_bytes());
            bytes[at + 8..at + 12].copy_from_slice(&value.to_le_bytes());
        }
        entry(&mut bytes, 10, 0x8825, 4, 1, 26);
        bytes[26..28].copy_from_slice(&4u16.to_le_bytes());
        entry(&mut bytes, 28, 1, 2, 2, u32::from(b'S'));
        entry(&mut bytes, 40, 2, 5, 3, 80);
        entry(&mut bytes, 52, 3, 2, 2, u32::from(b'W'));
        entry(&mut bytes, 64, 4, 5, 3, 104);
        for (index, numerator) in [12u32, 30, 0, 45, 15, 0].into_iter().enumerate() {
            let at = 80 + index * 8;
            bytes[at..at + 4].copy_from_slice(&numerator.to_le_bytes());
            bytes[at + 4..at + 8].copy_from_slice(&1u32.to_le_bytes());
        }
        assert_eq!(
            exif_metadata(&bytes)["location"],
            json!({"latitude":-12.5,"longitude":-45.25})
        );
        bytes[36] = 0;
        assert!(exif_metadata(&bytes).get("location").is_none());
        bytes[36] = b'S';
        bytes[84..88].fill(0);
        assert!(exif_metadata(&bytes).get("location").is_none());
    }

    #[test]
    fn paste_images_exif_ignores_malformed_offsets() {
        for bytes in [
            b"".as_slice(),
            b"Exif\0\0",
            b"II*\0\xff\xff\xff\xff",
            b"MM\0*\0\0\0\x08\xff\xff",
        ] {
            assert_eq!(exif_metadata(bytes), json!({}));
        }
    }
}
