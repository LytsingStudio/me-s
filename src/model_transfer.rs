use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{BufRead, Write},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use chrono::Local;
use serde::{Deserialize, Serialize};

use crate::{
    Result,
    config::{
        GlobalConfig, ModelConfig, config_home, create_private_directory, default_global_config,
        expand_home, user_home, write_private_atomic,
    },
};

const MAGIC: &[u8; 16] = b"ME-MODEL-EXPORT\0";
const FILE_VERSION: u32 = 1;
const PAYLOAD_VERSION: u32 = 1;
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;
const TAG_LEN: usize = 16;
const KDF_MEMORY_KIB: u32 = 19 * 1024;
const KDF_ITERATIONS: u32 = 2;
const KDF_PARALLELISM: u32 = 1;
const HEADER_LEN: usize = MAGIC.len() + 4 * 4 + SALT_LEN + NONCE_LEN;

#[derive(Debug, PartialEq, Eq)]
pub struct InitResult {
    pub config_file: PathBuf,
    pub reset: bool,
    pub cancelled: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ExportResult {
    pub file: PathBuf,
    pub models: usize,
    pub model_credentials: usize,
    pub codex_credential: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ImportResult {
    pub config_file: PathBuf,
    pub added: usize,
    pub overwritten: usize,
    pub model_credentials: usize,
    pub codex_credential: bool,
    pub default_model: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ArchivePayload {
    version: u32,
    exported_at_ms: u64,
    global_version: u32,
    default_model: String,
    models: Vec<ArchivedModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    codex_auth: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ArchivedModel {
    config: ModelConfig,
    credential: Option<String>,
}

pub fn initialize_global(input: &mut impl BufRead, output: &mut impl Write) -> Result<InitResult> {
    initialize_global_at(&config_home()?, input, output)
}

fn initialize_global_at(
    home: &Path,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<InitResult> {
    let config_file = home.join("conf.d/models.toml");
    let mut reset = false;
    if config_file.exists() {
        write!(
            output,
            "全局配置已存在。重新初始化会永久删除全部全局模型配置和凭据。请输入 YES 确认完全重置："
        )?;
        output.flush()?;
        let mut answer = String::new();
        input.read_line(&mut answer)?;
        if answer.trim() != "YES" {
            writeln!(output, "未重置全局配置。")?;
            return Ok(InitResult {
                config_file,
                reset: false,
                cancelled: true,
            });
        }
        ensure_safe_reset_target(home)?;
        fs::remove_dir_all(home)?;
        reset = true;
    }

    create_private_directory(home)?;
    create_private_directory(&home.join("credentials"))?;
    let config = default_global_config(home)?;
    config.save(&config_file)?;
    if reset {
        writeln!(output, "已完全重置全局配置：{}", config_file.display())?;
    } else {
        writeln!(output, "已初始化全局配置：{}", config_file.display())?;
    }
    Ok(InitResult {
        config_file,
        reset,
        cancelled: false,
    })
}

pub fn export_models(global: &GlobalConfig, password: &str) -> Result<ExportResult> {
    let output_directory = std::env::current_dir()?;
    let timestamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let codex_auth_file = config_home()?.join("codex/auth.json");
    export_models_at(
        global,
        &output_directory,
        password,
        &timestamp,
        &codex_auth_file,
    )
}

fn export_models_at(
    global: &GlobalConfig,
    output_directory: &Path,
    password: &str,
    timestamp: &str,
    codex_auth_file: &Path,
) -> Result<ExportResult> {
    validate_password(password)?;
    global.validate()?;

    let mut model_credentials = 0;
    let mut models = Vec::with_capacity(global.models.len());
    for model in &global.models {
        let credential = effective_model_credential(model)?;
        model_credentials += usize::from(credential.is_some());
        models.push(ArchivedModel {
            config: model.clone(),
            credential,
        });
    }

    let codex_auth = read_optional_text(codex_auth_file)?
        .filter(|content| !content.trim().is_empty())
        .map(validate_codex_auth)
        .transpose()?;
    let codex_credential = codex_auth.is_some();
    let payload = ArchivePayload {
        version: PAYLOAD_VERSION,
        exported_at_ms: now_ms(),
        global_version: global.version,
        default_model: global.default_model.clone(),
        models,
        codex_auth,
    };
    let plaintext = serde_json::to_vec(&payload)?;
    let encrypted = encrypt(&plaintext, password)?;
    let file = output_directory.join(format!("me-model-export-{timestamp}"));
    write_private_new(&file, &encrypted)?;
    Ok(ExportResult {
        file,
        models: global.models.len(),
        model_credentials,
        codex_credential,
    })
}

pub fn import_models(file: &Path, password: &str) -> Result<ImportResult> {
    let home = config_home()?;
    import_models_at(&home, file, password)
}

fn import_models_at(home: &Path, file: &Path, password: &str) -> Result<ImportResult> {
    validate_password(password)?;
    let encrypted = fs::read(file)?;
    let plaintext = decrypt(&encrypted, password)?;
    let payload: ArchivePayload = serde_json::from_slice(&plaintext)
        .map_err(|_| "model export payload is invalid or unsupported")?;
    if payload.version != PAYLOAD_VERSION {
        return Err(format!(
            "unsupported model export payload version {}",
            payload.version
        )
        .into());
    }
    let mut imported_models = Vec::with_capacity(payload.models.len());
    let mut credential_writes = BTreeMap::<PathBuf, Vec<u8>>::new();
    for archived in payload.models {
        let mut model = archived.config;
        if let Some(credential) = archived.credential {
            if credential.trim().is_empty() {
                return Err(
                    format!("model {} has an empty archived credential", model.name).into(),
                );
            }
            let digest = blake3::hash(credential.as_bytes()).to_hex();
            let path = home
                .join("credentials")
                .join(format!("model-credential-{digest}.key"));
            credential_writes
                .entry(path.clone())
                .or_insert_with(|| credential.into_bytes());
            model.api_key = None;
            model.api_key_env = None;
            model.credential_file = Some(path.to_string_lossy().into_owned());
        } else if model.credential_file.is_some() {
            model.credential_file = None;
        }
        imported_models.push(model);
    }

    let codex_auth = payload.codex_auth.map(validate_codex_auth).transpose()?;
    let imported = GlobalConfig {
        version: payload.global_version,
        default_model: payload.default_model,
        models: imported_models,
    };
    imported.validate()?;

    let config_file = home.join("conf.d/models.toml");
    let mut merged = if config_file.exists() {
        GlobalConfig::load(&config_file)?
    } else {
        GlobalConfig {
            version: imported.version,
            default_model: imported.default_model.clone(),
            models: Vec::new(),
        }
    };
    if merged.version != imported.version {
        return Err(format!(
            "cannot merge global config version {} into version {}",
            imported.version, merged.version
        )
        .into());
    }

    let mut added = 0;
    let mut overwritten = 0;
    for model in imported.models {
        if let Some(index) = merged
            .models
            .iter()
            .position(|candidate| candidate.name == model.name)
        {
            merged.models[index] = model;
            overwritten += 1;
        } else {
            merged.models.push(model);
            added += 1;
        }
    }
    merged.default_model = imported.default_model;
    merged.add_unset_effort();
    merged.validate()?;

    for (path, content) in &credential_writes {
        write_private_atomic(path, content)?;
    }
    let codex_auth_file = home.join("codex/auth.json");
    let codex_credential = if !codex_auth_file.exists()
        && let Some(codex_auth) = codex_auth
    {
        write_private_atomic(&codex_auth_file, codex_auth.as_bytes())?;
        true
    } else {
        false
    };
    merged.save(&config_file)?;

    Ok(ImportResult {
        config_file,
        added,
        overwritten,
        model_credentials: credential_writes.len(),
        codex_credential,
        default_model: merged.default_model,
    })
}

fn validate_codex_auth(content: String) -> Result<String> {
    let document: serde_json::Value =
        serde_json::from_str(&content).map_err(|_| "Codex OAuth credential is not valid JSON")?;
    if !document.is_object() {
        return Err("Codex OAuth credential root must be a JSON object".into());
    }
    Ok(content)
}

fn effective_model_credential(model: &ModelConfig) -> Result<Option<String>> {
    if let Some(key) = model.api_key.as_deref().filter(|key| !key.is_empty()) {
        return Ok(Some(key.to_owned()));
    }
    if let Some(name) = &model.api_key_env
        && let Ok(key) = std::env::var(name)
        && !key.is_empty()
    {
        return Ok(Some(key));
    }
    if let Some(path) = &model.credential_file {
        return read_optional_text(&expand_home(path))
            .map(|credential| credential.filter(|value| !value.trim().is_empty()));
    }
    Ok(None)
}

fn read_optional_text(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot read {}: {error}", path.display()).into()),
    }
}

fn validate_password(password: &str) -> Result<()> {
    if password.is_empty() {
        Err("model export password cannot be empty".into())
    } else {
        Ok(())
    }
}

fn encrypt(plaintext: &[u8], password: &str) -> Result<Vec<u8>> {
    let mut salt = [0_u8; SALT_LEN];
    let mut nonce = [0_u8; NONCE_LEN];
    getrandom::fill(&mut salt)?;
    getrandom::fill(&mut nonce)?;

    let mut header = Vec::with_capacity(HEADER_LEN);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&FILE_VERSION.to_le_bytes());
    header.extend_from_slice(&KDF_MEMORY_KIB.to_le_bytes());
    header.extend_from_slice(&KDF_ITERATIONS.to_le_bytes());
    header.extend_from_slice(&KDF_PARALLELISM.to_le_bytes());
    header.extend_from_slice(&salt);
    header.extend_from_slice(&nonce);

    let key = derive_key(
        password,
        &salt,
        KDF_MEMORY_KIB,
        KDF_ITERATIONS,
        KDF_PARALLELISM,
    )?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let nonce = XNonce::try_from(nonce.as_slice()).map_err(|_| "invalid encryption nonce")?;
    let ciphertext = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad: &header,
            },
        )
        .map_err(|_| "cannot encrypt model export")?;
    header.extend_from_slice(&ciphertext);
    Ok(header)
}

fn decrypt(file: &[u8], password: &str) -> Result<Vec<u8>> {
    if file.len() < HEADER_LEN + TAG_LEN || file.get(..MAGIC.len()) != Some(MAGIC) {
        return Err("file is not a supported me model export".into());
    }
    let version = read_u32(file, MAGIC.len())?;
    if version != FILE_VERSION {
        return Err(format!("unsupported model export file version {version}").into());
    }
    let memory = read_u32(file, MAGIC.len() + 4)?;
    let iterations = read_u32(file, MAGIC.len() + 8)?;
    let parallelism = read_u32(file, MAGIC.len() + 12)?;
    if !(8 * 1024..=256 * 1024).contains(&memory)
        || !(1..=10).contains(&iterations)
        || !(1..=16).contains(&parallelism)
    {
        return Err("model export contains unsafe key-derivation parameters".into());
    }
    let salt_start = MAGIC.len() + 16;
    let nonce_start = salt_start + SALT_LEN;
    let salt = &file[salt_start..nonce_start];
    let nonce = &file[nonce_start..HEADER_LEN];
    let key = derive_key(password, salt, memory, iterations, parallelism)?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let nonce = XNonce::try_from(nonce).map_err(|_| "model export nonce is invalid")?;
    cipher
        .decrypt(
            &nonce,
            Payload {
                msg: &file[HEADER_LEN..],
                aad: &file[..HEADER_LEN],
            },
        )
        .map_err(|_| "model export password is incorrect or the file is damaged".into())
}

fn derive_key(
    password: &str,
    salt: &[u8],
    memory: u32,
    iterations: u32,
    parallelism: u32,
) -> Result<[u8; KEY_LEN]> {
    let params = Params::new(memory, iterations, parallelism, Some(KEY_LEN))
        .map_err(|error| format!("invalid Argon2id parameters: {error}"))?;
    let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0_u8; KEY_LEN];
    argon2
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|error| format!("Argon2id key derivation failed: {error}"))?;
    Ok(key)
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let bytes = bytes
        .get(offset..offset + 4)
        .ok_or("model export header is truncated")?;
    Ok(u32::from_le_bytes(bytes.try_into()?))
}

fn write_private_new(path: &Path, content: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("cannot create export {}: {error}", path.display()))?;
    if let Err(error) = file.write_all(content).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error.into());
    }
    Ok(())
}

fn ensure_safe_reset_target(home: &Path) -> Result<()> {
    if !home.is_absolute()
        || home.parent().is_none()
        || home
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        || user_home().as_deref() == Some(home)
    {
        return Err(format!(
            "refusing to recursively reset unsafe config path {}",
            home.display()
        )
        .into());
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use crate::config::{ModelCapabilities, ProviderType};

    use super::*;

    fn temporary_directory(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "me-model-transfer-{name}-{}-{}",
            std::process::id(),
            now_ms()
        ))
    }

    fn model(name: &str, api_model: &str) -> ModelConfig {
        ModelConfig {
            name: name.into(),
            provider: ProviderType::OpenaiCompatible,
            reserve_output_context: true,
            base_url: "https://example.com/v1".into(),
            endpoint: "/chat/completions".into(),
            api_key: None,
            api_key_env: None,
            credential_file: None,
            model: api_model.into(),
            source_url: None,
            timeout_seconds: 30,
            capabilities: ModelCapabilities::default(),
            parameters: toml::Table::new(),
            effort_parameters: BTreeMap::new(),
        }
    }

    #[test]
    fn init_creates_defaults_declines_and_completely_resets() {
        let home = temporary_directory("init");
        let mut output = Vec::new();
        let result =
            initialize_global_at(&home, &mut Cursor::new(Vec::<u8>::new()), &mut output).unwrap();
        assert!(!result.reset);
        assert!(!result.cancelled);
        let config = GlobalConfig::load(&result.config_file).unwrap();
        assert_eq!(config.models.len(), 14);
        assert!(config.model("local-llama-server").is_some());
        assert_eq!(config.default_model, "cometapi-deepseek-v4-flash");

        let auth = home.join("codex/auth.json");
        let credential = home.join("credentials/cometapi.key");
        write_private_atomic(&auth, br#"{"tokens":{"access_token":"secret"}}"#).unwrap();
        write_private_atomic(&credential, b"comet-secret").unwrap();

        for answer in [
            b"\n".as_slice(),
            b"y\n".as_slice(),
            b"yes\n".as_slice(),
            b"Yes\n".as_slice(),
            b"NO\n".as_slice(),
        ] {
            let mut declined_output = Vec::new();
            let declined = initialize_global_at(
                &home,
                &mut Cursor::new(answer.to_vec()),
                &mut declined_output,
            )
            .unwrap();
            assert!(declined.cancelled);
            assert!(auth.exists());
            assert!(credential.exists());
            assert!(
                String::from_utf8(declined_output)
                    .unwrap()
                    .contains("请输入 YES 确认完全重置")
            );
        }

        let reset =
            initialize_global_at(&home, &mut Cursor::new(b"YES\n".to_vec()), &mut Vec::new())
                .unwrap();
        assert!(reset.reset);
        assert!(!reset.cancelled);
        assert!(!auth.exists());
        assert!(!credential.exists());
        assert_eq!(
            GlobalConfig::load(&reset.config_file)
                .unwrap()
                .default_model,
            "cometapi-deepseek-v4-flash"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn encrypted_export_import_merges_models_without_overwriting_target_codex_login() {
        let source = temporary_directory("source");
        let target = temporary_directory("target");
        let export_directory = temporary_directory("exports");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(&export_directory).unwrap();

        let shared_credential = source.join("shared.key");
        fs::write(&shared_credential, "shared-secret").unwrap();
        let mut first = model("first", "first-new");
        first.credential_file = Some(shared_credential.to_string_lossy().into_owned());
        let mut second = model("second", "second-new");
        second.api_key = Some("inline-secret".into());
        let global = GlobalConfig {
            version: 1,
            default_model: "second".into(),
            models: vec![first, second],
        };
        let source_auth = source.join("codex/auth.json");
        let exported_auth = r#"{"auth_mode":"chatgpt","tokens":{"access_token":"exported-oauth"}}"#;
        write_private_atomic(&source_auth, exported_auth.as_bytes()).unwrap();
        let exported = export_models_at(
            &global,
            &export_directory,
            "correct horse battery staple",
            "20260728-160356",
            &source_auth,
        )
        .unwrap();
        assert_eq!(
            exported.file.file_name().unwrap(),
            "me-model-export-20260728-160356"
        );
        assert_eq!(exported.models, 2);
        assert_eq!(exported.model_credentials, 2);
        assert!(exported.codex_credential);
        let encrypted = fs::read(&exported.file).unwrap();
        let payload: ArchivePayload =
            serde_json::from_slice(&decrypt(&encrypted, "correct horse battery staple").unwrap())
                .unwrap();
        assert_eq!(payload.codex_auth.as_deref(), Some(exported_auth));
        assert!(
            !encrypted
                .windows(b"shared-secret".len())
                .any(|window| window == b"shared-secret")
        );
        assert!(
            !encrypted
                .windows(b"exported-oauth".len())
                .any(|window| window == b"exported-oauth")
        );
        let mut old_second = model("second", "second-old");
        old_second.api_key = Some("old-secret".into());
        let existing = GlobalConfig {
            version: 1,
            default_model: "third".into(),
            models: vec![old_second, model("third", "third-kept")],
        };
        existing.save(&target.join("conf.d/models.toml")).unwrap();
        let target_auth = target.join("codex/auth.json");
        write_private_atomic(
            &target_auth,
            br#"{"auth_mode":"chatgpt","tokens":{"access_token":"device-local"}}"#,
        )
        .unwrap();

        let imported =
            import_models_at(&target, &exported.file, "correct horse battery staple").unwrap();
        assert_eq!(imported.added, 1);
        assert_eq!(imported.overwritten, 1);
        assert_eq!(imported.model_credentials, 2);
        assert!(!imported.codex_credential);
        assert_eq!(imported.default_model, "second");

        let merged = GlobalConfig::load(&target.join("conf.d/models.toml")).unwrap();
        assert_eq!(merged.default_model, "second");
        assert_eq!(merged.models.len(), 3);
        assert_eq!(merged.model("first").unwrap().model, "first-new");
        assert_eq!(merged.model("second").unwrap().model, "second-new");
        assert_eq!(merged.model("third").unwrap().model, "third-kept");
        assert_eq!(
            merged.model("first").unwrap().api_key().unwrap(),
            "shared-secret"
        );
        assert_eq!(
            merged.model("second").unwrap().api_key().unwrap(),
            "inline-secret"
        );
        assert!(
            fs::read_to_string(target_auth)
                .unwrap()
                .contains("device-local")
        );

        fs::remove_dir_all(source).unwrap();
        fs::remove_dir_all(target).unwrap();
        fs::remove_dir_all(export_directory).unwrap();
    }

    #[test]
    fn import_restores_codex_login_only_when_target_has_none() {
        let source = temporary_directory("codex-restore-source");
        let target = temporary_directory("codex-restore-target");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();
        let global = GlobalConfig {
            version: 1,
            default_model: "first".into(),
            models: vec![model("first", "first-api")],
        };
        let source_auth = source.join("codex/auth.json");
        let expected = r#"{"auth_mode":"chatgpt","tokens":{"access_token":"restored-oauth"}}"#;
        write_private_atomic(&source_auth, expected.as_bytes()).unwrap();
        let exported =
            export_models_at(&global, &source, "password", "codex-restore", &source_auth).unwrap();

        let imported = import_models_at(&target, &exported.file, "password").unwrap();
        assert!(imported.codex_credential);
        assert_eq!(
            fs::read_to_string(target.join("codex/auth.json")).unwrap(),
            expected
        );

        fs::remove_dir_all(source).unwrap();
        fs::remove_dir_all(target).unwrap();
    }

    #[test]
    fn wrong_password_corruption_and_unsafe_reset_do_not_mutate_config() {
        let source = temporary_directory("failure-source");
        let target = temporary_directory("failure-target");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();
        let mut secured = model("secured", "api-secured");
        secured.api_key = Some("secret".into());
        let global = GlobalConfig {
            version: 1,
            default_model: "secured".into(),
            models: vec![secured],
        };
        let exported = export_models_at(
            &global,
            &source,
            "right",
            "fixed",
            &source.join("codex/auth.json"),
        )
        .unwrap();
        let existing = GlobalConfig {
            version: 1,
            default_model: "existing".into(),
            models: vec![model("existing", "unchanged")],
        };
        let config_file = target.join("conf.d/models.toml");
        existing.save(&config_file).unwrap();
        let before = fs::read(&config_file).unwrap();

        assert!(import_models_at(&target, &exported.file, "wrong").is_err());
        assert_eq!(fs::read(&config_file).unwrap(), before);

        let mut damaged = fs::read(&exported.file).unwrap();
        let last = damaged.last_mut().unwrap();
        *last ^= 0x80;
        let damaged_file = source.join("damaged");
        fs::write(&damaged_file, damaged).unwrap();
        assert!(import_models_at(&target, &damaged_file, "right").is_err());
        assert_eq!(fs::read(&config_file).unwrap(), before);

        assert!(ensure_safe_reset_target(Path::new("/")).is_err());
        assert!(ensure_safe_reset_target(Path::new(".")).is_err());
        if let Some(home) = user_home() {
            assert!(ensure_safe_reset_target(&home).is_err());
        }
        assert!(
            export_models_at(
                &global,
                &source,
                "",
                "empty-password",
                &source.join("codex/auth.json"),
            )
            .is_err()
        );

        fs::remove_dir_all(source).unwrap();
        fs::remove_dir_all(target).unwrap();
    }

    #[test]
    fn import_bootstraps_a_missing_global_config_and_preserves_absent_codex_state() {
        let source = temporary_directory("bootstrap-source");
        let target = temporary_directory("bootstrap-target");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();

        let shared = source.join("shared.key");
        fs::write(&shared, "same-secret").unwrap();
        let mut first = model("first", "first-api");
        first.credential_file = Some(shared.to_string_lossy().into_owned());
        let mut second = model("second", "second-api");
        second.credential_file = Some(shared.to_string_lossy().into_owned());
        let global = GlobalConfig {
            version: 1,
            default_model: "first".into(),
            models: vec![first, second],
        };
        let exported = export_models_at(
            &global,
            &source,
            "password",
            "bootstrap",
            &source.join("codex/auth.json"),
        )
        .unwrap();

        let existing_auth = target.join("codex/auth.json");
        write_private_atomic(
            &existing_auth,
            br#"{"auth_mode":"chatgpt","tokens":{"access_token":"keep-me"}}"#,
        )
        .unwrap();
        let imported = import_models_at(&target, &exported.file, "password").unwrap();
        assert_eq!(imported.added, 2);
        assert_eq!(imported.overwritten, 0);
        assert_eq!(imported.model_credentials, 1);
        assert!(!imported.codex_credential);
        assert!(
            fs::read_to_string(existing_auth)
                .unwrap()
                .contains("keep-me")
        );

        let restored = GlobalConfig::load(&target.join("conf.d/models.toml")).unwrap();
        assert_eq!(restored.default_model, "first");
        assert_eq!(
            restored.model("first").unwrap().credential_file,
            restored.model("second").unwrap().credential_file
        );
        assert_eq!(
            restored.model("second").unwrap().api_key().unwrap(),
            "same-secret"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                exported.file.metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                target
                    .join("conf.d/models.toml")
                    .metadata()
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        fs::remove_dir_all(source).unwrap();
        fs::remove_dir_all(target).unwrap();
    }
}
