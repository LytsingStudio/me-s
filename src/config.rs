use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::Result;

const GLOBAL_CONFIG_VERSION: u32 = 1;
const WORKSPACE_CONFIG_VERSION: u32 = 2;
const MAX_MODEL_CONTEXT_WINDOW: u64 = 262_144;
pub const UNSET_EFFORT: &str = "unset";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderType {
    CodexOauth,
    OpenaiCompatible,
    Anthropic,
}

impl std::fmt::Display for ProviderType {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::CodexOauth => "codex-oauth",
            Self::OpenaiCompatible => "openai-compatible",
            Self::Anthropic => "anthropic",
        };
        formatter.write_str(value)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ModelCapabilities {
    pub context_window: u64,
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub input_modalities: Vec<String>,
    #[serde(default)]
    pub output_modalities: Vec<String>,
    #[serde(default)]
    pub reasoning_modes: Vec<String>,
    #[serde(default)]
    pub reasoning_efforts: Vec<String>,
    #[serde(default)]
    pub tools: bool,
    #[serde(default)]
    pub streaming: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelConfig {
    pub name: String,
    pub provider: ProviderType,
    #[serde(default)]
    pub reserve_output_context: bool,
    pub base_url: String,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub credential_file: Option<String>,
    pub model: String,
    pub source_url: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    pub capabilities: ModelCapabilities,
    #[serde(default)]
    pub parameters: toml::Table,
    #[serde(default)]
    pub effort_parameters: BTreeMap<String, toml::Table>,
}

fn default_timeout() -> u64 {
    120
}

impl ModelConfig {
    pub fn request_api_key(&self) -> Result<Option<String>> {
        if self.api_key.is_none() && self.api_key_env.is_none() && self.credential_file.is_none() {
            return Ok(None);
        }
        self.api_key().map(Some)
    }

    pub fn api_key(&self) -> Result<String> {
        if let Some(key) = self.api_key.as_deref().filter(|key| !key.is_empty()) {
            return Ok(key.to_owned());
        }

        if let Some(name) = &self.api_key_env
            && let Ok(key) = env::var(name)
            && !key.is_empty()
        {
            return Ok(key);
        }

        if let Some(path) = &self.credential_file {
            let key = fs::read_to_string(expand_home(path))?;
            let key = key.trim();
            if !key.is_empty() {
                return Ok(key.to_owned());
            }
        }

        Err(format!("model {} has no usable API credential", self.name).into())
    }

    pub fn validate_effort(&self, effort: &str) -> Result<()> {
        if effort == UNSET_EFFORT
            || self
                .capabilities
                .reasoning_efforts
                .iter()
                .any(|candidate| candidate == effort)
        {
            Ok(())
        } else {
            Err(format!("model {} does not support effort {effort}", self.name).into())
        }
    }

    pub fn output_token_reservation(&self, effort: Option<&str>) -> u64 {
        if !self.reserve_output_context {
            return 0;
        }
        effort
            .filter(|effort| *effort != UNSET_EFFORT)
            .and_then(|effort| self.effort_parameters.get(effort))
            .and_then(request_output_limit)
            .or_else(|| request_output_limit(&self.parameters))
            .or(self.capabilities.max_output_tokens)
            .unwrap_or(0)
    }
}

fn request_output_limit(parameters: &toml::Table) -> Option<u64> {
    ["max_output_tokens", "max_completion_tokens", "max_tokens"]
        .into_iter()
        .find_map(|name| {
            parameters
                .get(name)
                .and_then(toml::Value::as_integer)
                .and_then(|value| u64::try_from(value).ok())
        })
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GlobalConfig {
    pub version: u32,
    pub default_model: String,
    pub models: Vec<ModelConfig>,
}

impl GlobalConfig {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(format!(
                "global config {} does not exist; run `me-s init` first",
                path.display()
            )
            .into());
        }
        let mut config: Self = toml::from_str(&fs::read_to_string(path)?)?;
        for model in &mut config.models {
            let configured = model.capabilities.context_window;
            if configured > MAX_MODEL_CONTEXT_WINDOW {
                eprintln!(
                    "warning: model preset \"{}\" sets context_window to {} tokens; using the maximum of {} tokens",
                    model.name.escape_default(),
                    configured,
                    MAX_MODEL_CONTEXT_WINDOW
                );
                model.capabilities.context_window = MAX_MODEL_CONTEXT_WINDOW;
            }
        }
        config.add_unset_effort();
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        write_private_atomic(path, toml::to_string_pretty(self)?.as_bytes())
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != GLOBAL_CONFIG_VERSION {
            return Err(format!("unsupported global config version {}", self.version).into());
        }
        let mut names = HashSet::new();
        for model in &self.models {
            if !names.insert(model.name.as_str()) {
                return Err(format!("duplicate model name {}", model.name).into());
            }
        }
        if self.model(&self.default_model).is_none() {
            return Err(format!("default model {} does not exist", self.default_model).into());
        }
        Ok(())
    }

    pub fn model(&self, name: &str) -> Option<&ModelConfig> {
        self.models.iter().find(|model| model.name == name)
    }

    pub fn add_unset_effort(&mut self) {
        for model in &mut self.models {
            model
                .capabilities
                .reasoning_efforts
                .retain(|effort| effort != UNSET_EFFORT);
            model
                .capabilities
                .reasoning_efforts
                .insert(0, UNSET_EFFORT.to_owned());
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkspaceConfig {
    pub version: u32,
    pub model: String,
    pub effort: String,
    pub orchestrator: String,
}

impl WorkspaceConfig {
    pub fn create(path: &Path, model: String) -> Result<Self> {
        if path.exists() {
            let content = fs::read_to_string(path)?;
            if let Ok(config) = toml::from_str::<Self>(&content)
                && config.validate().is_ok()
            {
                return Err(format!("workspace config {} already exists", path.display()).into());
            }
        }
        let config = Self {
            version: WORKSPACE_CONFIG_VERSION,
            model,
            effort: UNSET_EFFORT.to_owned(),
            orchestrator: default_orchestrator(),
        };
        config.save(path)?;
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Err(format!(
                "workspace config {} does not exist; run `me create` first",
                path.display()
            )
            .into());
        }
        let config: Self = toml::from_str(&fs::read_to_string(path)?)?;
        config.validate()?;
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        write_private_atomic(path, toml::to_string_pretty(self)?.as_bytes())
    }

    pub fn selected_model<'a>(&'a self, global: &'a GlobalConfig) -> Result<&'a ModelConfig> {
        global
            .model(&self.model)
            .ok_or_else(|| format!("selected model {} does not exist", self.model).into())
    }

    fn validate(&self) -> Result<()> {
        if self.version != WORKSPACE_CONFIG_VERSION {
            return Err(format!("unsupported workspace config version {}", self.version).into());
        }
        if self.model.is_empty() {
            return Err("workspace model is empty".into());
        }
        if self.effort.is_empty() {
            return Err("workspace effort is empty".into());
        }
        if self.orchestrator.is_empty() {
            return Err("workspace orchestrator is empty".into());
        }
        Ok(())
    }
}

fn default_orchestrator() -> String {
    "main-agent".into()
}

pub fn config_home() -> Result<PathBuf> {
    if let Some(path) = env::var_os("ME_CONFIG_HOME") {
        return Ok(PathBuf::from(path));
    }
    #[cfg(windows)]
    {
        if let Some(path) = env::var_os("APPDATA") {
            return Ok(PathBuf::from(path).join("me"));
        }
        user_home()
            .map(|home| home.join("AppData").join("Roaming").join("me"))
            .ok_or_else(|| "APPDATA and USERPROFILE are not set".into())
    }
    #[cfg(not(windows))]
    {
        if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
            return Ok(PathBuf::from(path).join("me"));
        }
        let home = user_home().ok_or("HOME is not set")?;
        Ok(home.join(".config/me"))
    }
}

pub fn global_config_path() -> Result<PathBuf> {
    Ok(config_home()?.join("conf.d/models.toml"))
}

pub fn default_global_config(home: &Path) -> Result<GlobalConfig> {
    let mut config: GlobalConfig = toml::from_str(include_str!("default_models.toml"))?;
    for model in &mut config.models {
        if model.base_url == "https://api.cometapi.com/v1" {
            model.credential_file = Some(
                home.join("credentials/cometapi.key")
                    .to_string_lossy()
                    .into_owned(),
            );
        } else if model.base_url == "https://api.xiaomimimo.com/v1" {
            model.credential_file = Some(
                home.join("credentials/xiaomi-mimo.key")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    config.validate()?;
    Ok(config)
}

pub fn workspace_config_path(workspace: &Path) -> PathBuf {
    workspace.join(".me/config.toml")
}

pub fn workspace_edb_path(workspace: &Path) -> PathBuf {
    workspace.join(".me/edb/main.edb")
}

pub fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        return user_home().unwrap_or_default();
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"))
        && let Some(home) = user_home()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

pub(crate) fn user_home() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        env::var_os("USERPROFILE").map(PathBuf::from).or_else(|| {
            let drive = env::var_os("HOMEDRIVE")?;
            let path = env::var_os("HOMEPATH")?;
            let mut home = PathBuf::from(drive);
            home.push(path);
            Some(home)
        })
    }
    #[cfg(not(windows))]
    {
        env::var_os("HOME").map(PathBuf::from)
    }
}

pub(crate) fn create_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(crate) fn write_private_atomic(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_private_directory(parent)?;
    }
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = path.with_extension(format!("tmp-{}-{suffix}", std::process::id()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    if let Err(error) = file.write_all(content).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    drop(file);
    if let Err(error) = replace_file(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, path: &Path) -> std::io::Result<()> {
    fs::rename(temporary, path)
}

#[cfg(windows)]
fn replace_file(temporary: &Path, path: &Path) -> std::io::Result<()> {
    if !path.exists() {
        return fs::rename(temporary, path);
    }
    let backup = path.with_extension(format!("backup-{}", std::process::id()));
    if backup.exists() {
        fs::remove_file(&backup)?;
    }
    fs::rename(path, &backup)?;
    match fs::rename(temporary, path) {
        Ok(()) => fs::remove_file(backup),
        Err(error) => {
            let _ = fs::rename(backup, path);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> ModelConfig {
        ModelConfig {
            name: "test".into(),
            provider: ProviderType::OpenaiCompatible,
            reserve_output_context: true,
            base_url: "https://example.com/v1".into(),
            endpoint: "/chat/completions".into(),
            api_key: Some("key".into()),
            api_key_env: None,
            credential_file: None,
            model: "api-model".into(),
            source_url: None,
            timeout_seconds: 1,
            capabilities: ModelCapabilities::default(),
            parameters: toml::Table::new(),
            effort_parameters: BTreeMap::new(),
        }
    }

    fn temporary_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("me-{name}-{}", std::process::id()))
    }

    #[test]
    fn global_and_workspace_config_round_trip() {
        let directory = temporary_path("config");
        let global_path = directory.join("models.toml");
        let workspace_path = directory.join("workspace.toml");
        let global = GlobalConfig {
            version: 1,
            default_model: "test".into(),
            models: vec![model()],
        };
        global.save(&global_path).unwrap();

        let loaded = GlobalConfig::load(&global_path).unwrap();
        assert_eq!(loaded.model("test").unwrap().model, "api-model");
        assert_eq!(
            loaded.model("test").unwrap().capabilities.reasoning_efforts,
            vec![UNSET_EFFORT]
        );

        assert!(WorkspaceConfig::load(&workspace_path).is_err());
        WorkspaceConfig::create(&workspace_path, "test".into()).unwrap();
        assert!(WorkspaceConfig::create(&workspace_path, "test".into()).is_err());
        let loaded_workspace = WorkspaceConfig::load(&workspace_path).unwrap();
        assert_eq!(loaded_workspace.orchestrator, "main-agent");
        assert_eq!(loaded_workspace.effort, UNSET_EFFORT);
        assert_eq!(
            loaded_workspace.selected_model(&loaded).unwrap().name,
            "test"
        );

        let legacy_path = directory.join("legacy.toml");
        fs::write(&legacy_path, "version = 1\nmodel = \"test\"\n").unwrap();
        assert!(WorkspaceConfig::load(&legacy_path).is_err());
        WorkspaceConfig::create(&legacy_path, "test".into()).unwrap();
        assert_eq!(
            WorkspaceConfig::load(&legacy_path).unwrap().orchestrator,
            "main-agent"
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn global_config_load_clamps_context_windows_without_rewriting_file() {
        let directory = temporary_path("context-window-limit");
        let path = directory.join("models.toml");
        let mut above = model();
        above.name = "模型-above".into();
        above.capabilities.context_window = MAX_MODEL_CONTEXT_WINDOW + 1;
        let mut exact = model();
        exact.name = "exact".into();
        exact.capabilities.context_window = MAX_MODEL_CONTEXT_WINDOW;
        let mut below = model();
        below.name = "below".into();
        below.capabilities.context_window = MAX_MODEL_CONTEXT_WINDOW - 1;
        let config = GlobalConfig {
            version: GLOBAL_CONFIG_VERSION,
            default_model: above.name.clone(),
            models: vec![above, exact, below],
        };
        config.save(&path).unwrap();
        let original = fs::read(&path).unwrap();

        let loaded = GlobalConfig::load(&path).unwrap();

        assert_eq!(
            loaded
                .model("模型-above")
                .unwrap()
                .capabilities
                .context_window,
            MAX_MODEL_CONTEXT_WINDOW
        );
        assert_eq!(
            loaded.model("exact").unwrap().capabilities.context_window,
            MAX_MODEL_CONTEXT_WINDOW
        );
        assert_eq!(
            loaded.model("below").unwrap().capabilities.context_window,
            MAX_MODEL_CONTEXT_WINDOW - 1
        );
        assert_eq!(fs::read(&path).unwrap(), original);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn model_without_reservation_policy_does_not_reserve_output_context() {
        let serialized = toml::to_string(&model()).unwrap();
        let legacy = serialized.replace("reserve_output_context = true\n", "");
        let loaded: ModelConfig = toml::from_str(&legacy).unwrap();
        assert!(!loaded.reserve_output_context);
    }

    #[test]
    fn built_in_models_request_their_declared_maximum_output() {
        let directory = temporary_path("default-output-limits");
        let config = default_global_config(&directory).unwrap();

        for model in config.models {
            if model.name == "local-llama-server" {
                assert!(!model.reserve_output_context);
                assert_eq!(model.capabilities.max_output_tokens, None);
                assert!(model.parameters.is_empty());
                continue;
            }
            assert!(
                model.reserve_output_context,
                "{} must explicitly reserve its configured output budget",
                model.name
            );
            let declared = model
                .capabilities
                .max_output_tokens
                .unwrap_or_else(|| panic!("{} has no declared output limit", model.name));
            let requested = ["max_output_tokens", "max_completion_tokens", "max_tokens"]
                .into_iter()
                .find_map(|name| model.parameters.get(name).and_then(toml::Value::as_integer))
                .unwrap_or_else(|| panic!("{} has no request output limit", model.name));
            assert_eq!(
                requested,
                i64::try_from(declared).unwrap(),
                "{} does not request its declared maximum output",
                model.name
            );
        }
    }

    #[test]
    fn built_in_local_llama_server_is_credentialless_and_unset_only() {
        let directory = temporary_path("default-local-llama-server");
        let config = default_global_config(&directory).unwrap();
        let model = config.model("local-llama-server").unwrap();

        assert_eq!(model.base_url, "http://39.108.58.109:8000/");
        assert_eq!(model.endpoint, "/v1/chat/completions");
        assert_eq!(model.model, "local");
        assert_eq!(model.capabilities.context_window, 262_144);
        assert_eq!(model.capabilities.reasoning_efforts, vec![UNSET_EFFORT]);
        assert_eq!(model.request_api_key().unwrap(), None);
    }

    #[test]
    fn built_in_deepseek_models_use_the_shared_output_limit() {
        let directory = temporary_path("default-deepseek-output-limits");
        let config = default_global_config(&directory).unwrap();
        let deepseek_models = config
            .models
            .iter()
            .filter(|model| model.name.contains("deepseek"))
            .collect::<Vec<_>>();

        assert!(!deepseek_models.is_empty());
        for model in deepseek_models {
            assert_eq!(model.capabilities.max_output_tokens, Some(262_144));
            assert_eq!(
                model
                    .parameters
                    .get("max_tokens")
                    .and_then(toml::Value::as_integer),
                Some(262_144)
            );
        }
    }

    #[test]
    fn built_in_xiaomi_mimo_models_use_their_shared_private_credential() {
        let directory = temporary_path("default-xiaomi-credential");
        let config = default_global_config(&directory).unwrap();
        let expected = directory
            .join("credentials/xiaomi-mimo.key")
            .to_string_lossy()
            .into_owned();

        for name in ["xiaomi-mimi-v2.5-pro", "xiaomi-mimo-v2.5"] {
            let model = config.model(name).unwrap();
            assert_eq!(model.credential_file.as_deref(), Some(expected.as_str()));
            assert_eq!(model.capabilities.context_window, 1_048_576);
            assert_eq!(model.capabilities.max_output_tokens, Some(131_072));
        }
    }
}
