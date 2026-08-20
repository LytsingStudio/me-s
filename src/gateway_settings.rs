use std::{collections::HashMap, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    Result,
    config::{GlobalConfig, ModelCapabilities, ModelConfig, ProviderType},
};

#[derive(Clone, Deserialize, Serialize)]
pub struct GatewaySettings {
    pub version: u32,
    pub default_model: String,
    pub models: Vec<GatewayModelSettings>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct GatewayModelSettings {
    pub original_name: Option<String>,
    pub name: String,
    pub provider: ProviderType,
    pub reserve_output_context: bool,
    pub base_url: String,
    pub endpoint: String,
    pub api_key_env: Option<String>,
    pub credential_file: Option<String>,
    pub model: String,
    pub source_url: Option<String>,
    pub timeout_seconds: u64,
    pub capabilities: ModelCapabilities,
    pub parameters: toml::Table,
    pub effort_parameters: std::collections::BTreeMap<String, toml::Table>,
    pub has_inline_api_key: bool,
    #[serde(default, skip_serializing)]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing)]
    pub clear_inline_api_key: bool,
}

impl GatewaySettings {
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self::from_global(&GlobalConfig::load(path)?))
    }

    pub fn save(self, path: &Path) -> Result<Self> {
        let existing = GlobalConfig::load(path)?;
        let updated = self.into_global(&existing)?;
        updated.save(path)?;
        Ok(Self::from_global(&updated))
    }

    fn from_global(global: &GlobalConfig) -> Self {
        Self {
            version: global.version,
            default_model: global.default_model.clone(),
            models: global
                .models
                .iter()
                .map(|model| GatewayModelSettings {
                    original_name: Some(model.name.clone()),
                    name: model.name.clone(),
                    provider: model.provider.clone(),
                    reserve_output_context: model.reserve_output_context,
                    base_url: model.base_url.clone(),
                    endpoint: model.endpoint.clone(),
                    api_key_env: model.api_key_env.clone(),
                    credential_file: model.credential_file.clone(),
                    model: model.model.clone(),
                    source_url: model.source_url.clone(),
                    timeout_seconds: model.timeout_seconds,
                    capabilities: model.capabilities.clone(),
                    parameters: model.parameters.clone(),
                    effort_parameters: model.effort_parameters.clone(),
                    has_inline_api_key: model.api_key.as_ref().is_some_and(|key| !key.is_empty()),
                    api_key: None,
                    clear_inline_api_key: false,
                })
                .collect(),
        }
    }

    fn into_global(self, existing: &GlobalConfig) -> Result<GlobalConfig> {
        if self.version != existing.version {
            return Err(format!(
                "global settings version changed from {} to {}",
                existing.version, self.version
            )
            .into());
        }
        let existing = existing
            .models
            .iter()
            .map(|model| (model.name.as_str(), model))
            .collect::<HashMap<_, _>>();
        let models = self
            .models
            .into_iter()
            .map(|model| {
                let previous_secret = model
                    .original_name
                    .as_deref()
                    .and_then(|name| existing.get(name))
                    .and_then(|model| model.api_key.clone());
                let api_key = if model.clear_inline_api_key {
                    None
                } else if let Some(api_key) = model.api_key {
                    if api_key.is_empty() {
                        return Err(format!(
                            "model {} submitted an empty API Key; omit it to preserve or explicitly clear it",
                            model.name
                        )
                        .into());
                    }
                    Some(api_key)
                } else {
                    previous_secret
                };
                Ok(ModelConfig {
                    name: model.name,
                    provider: model.provider,
                    reserve_output_context: model.reserve_output_context,
                    base_url: model.base_url,
                    endpoint: model.endpoint,
                    api_key,
                    api_key_env: model.api_key_env,
                    credential_file: model.credential_file,
                    model: model.model,
                    source_url: model.source_url,
                    timeout_seconds: model.timeout_seconds,
                    capabilities: model.capabilities,
                    parameters: model.parameters,
                    effort_parameters: model.effort_parameters,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let global = GlobalConfig {
            version: self.version,
            default_model: self.default_model,
            models,
        };
        global.validate()?;
        Ok(global)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::default_global_config;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn config() -> GlobalConfig {
        let home = std::env::temp_dir().join(format!(
            "me-gateway-settings-source-{}",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        default_global_config(&home).unwrap()
    }

    #[test]
    fn serialized_settings_never_include_an_existing_inline_secret() {
        let mut global = config();
        global.models[0].api_key = Some("super-secret".into());
        let settings = GatewaySettings::from_global(&global);
        let json = serde_json::to_string(&settings).unwrap();
        assert!(!json.contains("super-secret"));
        assert!(!json.contains("\"api_key\""));
        assert!(json.contains("has_inline_api_key"));
    }

    #[test]
    fn omitted_secret_is_preserved_and_explicit_clear_is_honored() {
        let mut global = config();
        global.models[0].api_key = Some("keep-me".into());
        let mut settings = GatewaySettings::from_global(&global);
        let preserved = settings.clone().into_global(&global).unwrap();
        assert_eq!(preserved.models[0].api_key.as_deref(), Some("keep-me"));
        settings.models[0].clear_inline_api_key = true;
        let cleared = settings.into_global(&global).unwrap();
        assert!(cleared.models[0].api_key.is_none());
    }

    #[test]
    fn model_rename_preserves_the_old_secret_and_explicit_replacement_wins() {
        let mut global = config();
        let original = global.models[0].name.clone();
        global.default_model = original.clone();
        global.models[0].api_key = Some("old-secret".into());

        let mut renamed = GatewaySettings::from_global(&global);
        renamed.default_model = "renamed-model".into();
        renamed.models[0].name = "renamed-model".into();
        let preserved = renamed.clone().into_global(&global).unwrap();
        assert_eq!(preserved.default_model, "renamed-model");
        assert_eq!(preserved.models[0].api_key.as_deref(), Some("old-secret"));

        renamed.models[0].api_key = Some("new-secret".into());
        let replaced = renamed.into_global(&global).unwrap();
        assert_eq!(replaced.models[0].api_key.as_deref(), Some("new-secret"));
    }
}
