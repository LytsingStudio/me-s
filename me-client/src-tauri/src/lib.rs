mod cache;
mod gateway;

use std::{collections::BTreeMap, env, io, path::PathBuf};

use cache::{CacheChunk, CacheDatabase, CacheMetadata, CacheSaveRequest};
use gateway::{DownloadResult, GatewayRequest, GatewayResponse, GatewayTransport};
use serde::Serialize;
use tauri::{AppHandle, Manager, State};

const ENDPOINT_SETTING: &str = "gateway.endpoint";
const DEVICE_PREFERENCE_KEYS: [&str; 3] = ["me-theme", "me-color-mode", "me-send-shortcut"];

struct AppState {
    cache: CacheDatabase,
    gateway: GatewayTransport,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClientBootstrap {
    endpoint: Option<String>,
    device_preferences: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct ConfiguredTarget {
    endpoint: String,
}

fn valid_device_preference(key: &str, value: &str) -> bool {
    match key {
        "me-theme" => matches!(
            value,
            "violet"
                | "graphite"
                | "ocean"
                | "forest"
                | "sand"
                | "aurora"
                | "sakura"
                | "neon"
                | "obsidian"
        ),
        "me-color-mode" => matches!(value, "light" | "dark"),
        "me-send-shortcut" => matches!(value, "enter" | "modified-enter"),
        _ => false,
    }
}

fn load_device_preferences(cache: &CacheDatabase) -> Result<BTreeMap<String, String>, String> {
    let mut preferences = BTreeMap::new();
    for key in DEVICE_PREFERENCE_KEYS {
        if let Some(value) = cache.setting(key)? {
            if valid_device_preference(key, &value) {
                preferences.insert(key.to_owned(), value);
            }
        }
    }
    Ok(preferences)
}

#[tauri::command]
async fn client_bootstrap(state: State<'_, AppState>) -> Result<ClientBootstrap, String> {
    let endpoint = state.gateway.endpoint().await;
    let cache = state.cache.clone();
    let device_preferences = run_blocking(move || load_device_preferences(&cache)).await?;
    Ok(ClientBootstrap {
        endpoint,
        device_preferences,
    })
}

#[tauri::command]
async fn configure_target(
    endpoint: String,
    state: State<'_, AppState>,
) -> Result<ConfiguredTarget, String> {
    let endpoint = state.gateway.configure(&endpoint).await?;
    let cache = state.cache.clone();
    let stored_endpoint = endpoint.clone();
    run_blocking(move || cache.set_setting(ENDPOINT_SETTING, &stored_endpoint)).await?;
    Ok(ConfiguredTarget { endpoint })
}

#[tauri::command]
async fn set_device_preference(
    key: String,
    value: String,
    state: State<'_, AppState>,
) -> Result<(), String> {
    if !valid_device_preference(&key, &value) {
        return Err("设备偏好无效".into());
    }
    let cache = state.cache.clone();
    run_blocking(move || cache.set_setting(&key, &value)).await
}

#[tauri::command]
async fn gateway_request(
    request: GatewayRequest,
    state: State<'_, AppState>,
) -> Result<GatewayResponse, String> {
    state.gateway.request(request).await
}

#[tauri::command]
async fn cache_load_metadata(
    edb_ids: Vec<String>,
    state: State<'_, AppState>,
) -> Result<Vec<CacheMetadata>, String> {
    let cache = state.cache.clone();
    run_blocking(move || cache.load_metadata(&edb_ids)).await
}

#[tauri::command]
async fn cache_load_chunk(
    edb_id: String,
    start_order: u64,
    byte_limit: u64,
    state: State<'_, AppState>,
) -> Result<CacheChunk, String> {
    let cache = state.cache.clone();
    run_blocking(move || cache.load_chunk(&edb_id, start_order, byte_limit)).await
}

#[tauri::command]
async fn cache_list(state: State<'_, AppState>) -> Result<Vec<CacheMetadata>, String> {
    let cache = state.cache.clone();
    run_blocking(move || cache.list()).await
}

#[tauri::command]
async fn cache_save_batch(
    session: CacheSaveRequest,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let cache = state.cache.clone();
    run_blocking(move || cache.save(session)).await
}

#[tauri::command]
async fn cache_remove(edb_id: String, state: State<'_, AppState>) -> Result<(), String> {
    let cache = state.cache.clone();
    run_blocking(move || cache.remove(&edb_id)).await
}

#[tauri::command]
async fn download_file(
    path: String,
    filename: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<DownloadResult, String> {
    let directory = app
        .path()
        .download_dir()
        .map_err(|error| format!("无法确定下载目录：{error}"))?;
    state.gateway.download(&path, &filename, &directory).await
}

async fn run_blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(operation)
        .await
        .map_err(|error| format!("后台任务未能完成：{error}"))?
}

fn app_state(app: &tauri::App) -> Result<AppState, io::Error> {
    let data_directory = match env::var_os("ME_CLIENT_DATA_DIR") {
        Some(directory) => PathBuf::from(directory),
        None => app.path().app_data_dir().map_err(io::Error::other)?,
    };
    let cache = CacheDatabase::new(database_path(data_directory)).map_err(io::Error::other)?;
    let endpoint = cache.setting(ENDPOINT_SETTING).map_err(io::Error::other)?;
    let gateway = GatewayTransport::new(endpoint).map_err(io::Error::other)?;
    Ok(AppState { cache, gateway })
}

fn database_path(data_directory: PathBuf) -> PathBuf {
    data_directory.join("me-client.sqlite3")
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            app.manage(app_state(app)?);
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            client_bootstrap,
            configure_target,
            set_device_preference,
            gateway_request,
            cache_load_metadata,
            cache_load_chunk,
            cache_list,
            cache_save_batch,
            cache_remove,
            download_file,
        ])
        .run(tauri::generate_context!())
        .expect("error while running ME Client");
}

#[cfg(test)]
mod tests {
    use super::valid_device_preference;

    #[test]
    fn device_preferences_accept_only_the_fixed_global_ui_values() {
        for theme in [
            "violet", "graphite", "ocean", "forest", "sand", "aurora", "sakura", "neon", "obsidian",
        ] {
            assert!(valid_device_preference("me-theme", theme));
        }
        assert!(valid_device_preference("me-color-mode", "light"));
        assert!(valid_device_preference("me-color-mode", "dark"));
        assert!(valid_device_preference("me-send-shortcut", "enter"));
        assert!(valid_device_preference(
            "me-send-shortcut",
            "modified-enter"
        ));
        assert!(!valid_device_preference("me-theme", "unknown"));
        assert!(!valid_device_preference(
            "gateway.endpoint",
            "https://example.com"
        ));
        assert!(!valid_device_preference("me-send-shortcut", "unknown"));
    }
}
