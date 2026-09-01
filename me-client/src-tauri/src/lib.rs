mod cache;
mod gateway;

use std::{
    collections::BTreeMap,
    env, io,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};

#[cfg(target_os = "windows")]
use std::{ffi::c_void, ptr};

#[cfg(target_os = "macos")]
use objc2::{
    MainThreadMarker, ffi, msg_send,
    rc::Retained,
    runtime::{AnyClass, AnyObject, Imp, Sel},
    sel,
};
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSApplication, NSMenu, NSMenuItem};
#[cfg(target_os = "macos")]
use objc2_foundation::NSString;

use cache::{CacheChunk, CacheDatabase, CacheMetadata, CacheSaveRequest, RememberedDevice};
use gateway::{
    DownloadResult, GatewayRequest, GatewayResponse, GatewayTransport, LocalDevice,
    discover_local_device, normalize_endpoint, online_remembered_devices,
};
use serde::Serialize;
use tauri::{AppHandle, Manager, State, WebviewWindow};

const ENDPOINT_SETTING: &str = "gateway.endpoint";
const DEVICE_PREFERENCE_KEYS: [&str; 3] = ["me-theme", "me-color-mode", "me-send-shortcut"];

const MAX_REMEMBERED_PASSWORD_BYTES: usize = 4096;

struct AppState {
    cache: CacheDatabase,
    gateway: GatewayTransport,
    window_revealed: AtomicBool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClientBootstrap {
    endpoint: Option<String>,
    device_preferences: BTreeMap<String, String>,
    remembered_devices: Vec<RememberedDeviceStatus>,
    local_device: LocalDevice,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RememberedDeviceStatus {
    endpoint: String,
    password: String,
    updated_at: u64,
    online: bool,
}

impl RememberedDeviceStatus {
    fn from_device(device: RememberedDevice, online: bool) -> Self {
        Self {
            endpoint: device.endpoint,
            password: device.password,
            updated_at: device.updated_at,
            online,
        }
    }
}

#[derive(Serialize)]
struct ConfiguredTarget {
    endpoint: String,
}

fn normalized_window_title(value: Option<String>) -> String {
    let value = value.unwrap_or_default();
    let value = value.trim();
    if value.is_empty() {
        "ME Client".into()
    } else {
        value.chars().take(256).collect()
    }
}

#[cfg(target_os = "macos")]
fn apply_platform_window_shape(
    window: &WebviewWindow,
    state: &ClientWindowState,
) -> Result<(), String> {
    let ns_window = window.ns_window().map_err(|error| error.to_string())? as *mut AnyObject;
    if ns_window.is_null() {
        return Err("macOS window is unavailable".into());
    }
    let corner_radius = if state.maximized || state.fullscreen {
        0.0
    } else {
        18.0
    };
    unsafe {
        let content_view: *mut AnyObject = msg_send![ns_window, contentView];
        if content_view.is_null() {
            return Err("macOS content view is unavailable".into());
        }
        let _: () = msg_send![content_view, setWantsLayer: true];
        let layer: *mut AnyObject = msg_send![content_view, layer];
        if layer.is_null() {
            return Err("macOS content layer is unavailable".into());
        }
        let curve = NSString::from_str("continuous");
        let _: () = msg_send![layer, setCornerCurve: &*curve];
        let _: () = msg_send![layer, setCornerRadius: corner_radius];
        let _: () = msg_send![layer, setMasksToBounds: true];
        let _: () = msg_send![ns_window, setHasShadow: true];
    }
    Ok(())
}

#[cfg(target_os = "windows")]
#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmSetWindowAttribute(
        hwnd: *mut c_void,
        attribute: u32,
        value: *const c_void,
        value_size: u32,
    ) -> i32;
}

#[cfg(target_os = "windows")]
#[link(name = "gdi32")]
unsafe extern "system" {
    fn CreateRoundRectRgn(
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        ellipse_width: i32,
        ellipse_height: i32,
    ) -> *mut c_void;
    fn DeleteObject(object: *mut c_void) -> i32;
}

#[cfg(target_os = "windows")]
#[link(name = "user32")]
unsafe extern "system" {
    fn SetWindowRgn(hwnd: *mut c_void, region: *mut c_void, redraw: i32) -> i32;
}

#[cfg(target_os = "windows")]
fn apply_windows_window_region(
    window: &WebviewWindow,
    state: &ClientWindowState,
    hwnd: *mut c_void,
) -> Result<(), String> {
    if state.maximized || state.fullscreen {
        let changed = unsafe { SetWindowRgn(hwnd, ptr::null_mut(), 1) };
        if changed == 0 {
            return Err(format!(
                "unable to clear the Windows window region: {}",
                io::Error::last_os_error()
            ));
        }
        return Ok(());
    }

    let size = window.outer_size().map_err(|error| error.to_string())?;
    let scale = window.scale_factor().map_err(|error| error.to_string())?;
    let width = i32::try_from(size.width).map_err(|_| "Windows window width is too large")?;
    let height = i32::try_from(size.height).map_err(|_| "Windows window height is too large")?;
    let radius = (18.0 * scale).round().max(1.0) as i32;
    let diameter = radius.saturating_mul(2);
    let region = unsafe {
        CreateRoundRectRgn(
            0,
            0,
            width.saturating_add(1),
            height.saturating_add(1),
            diameter,
            diameter,
        )
    };
    if region.is_null() {
        return Err(format!(
            "unable to create the Windows rounded window region: {}",
            io::Error::last_os_error()
        ));
    }
    let changed = unsafe { SetWindowRgn(hwnd, region, 1) };
    if changed == 0 {
        unsafe {
            let _ = DeleteObject(region);
        }
        return Err(format!(
            "unable to apply the Windows rounded window region: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn apply_platform_window_shape(
    window: &WebviewWindow,
    state: &ClientWindowState,
) -> Result<(), String> {
    const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
    const DWMWCP_DONOTROUND: u32 = 1;
    const DWMWCP_ROUND: u32 = 2;
    let preference = if state.maximized || state.fullscreen {
        DWMWCP_DONOTROUND
    } else {
        DWMWCP_ROUND
    };
    let hwnd = window.hwnd().map_err(|error| error.to_string())?;
    apply_windows_window_region(window, state, hwnd.0)?;
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd.0,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&preference as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        );
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn apply_platform_window_shape(
    _window: &WebviewWindow,
    _state: &ClientWindowState,
) -> Result<(), String> {
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClientWindowState {
    maximized: bool,
    fullscreen: bool,
}

fn client_window_state(window: &WebviewWindow) -> Result<ClientWindowState, String> {
    let state = ClientWindowState {
        maximized: window.is_maximized().map_err(|error| error.to_string())?,
        fullscreen: window.is_fullscreen().map_err(|error| error.to_string())?,
    };
    apply_platform_window_shape(window, &state)?;
    Ok(state)
}

#[tauri::command]
fn client_window_action(
    action: String,
    value: Option<String>,
    window: WebviewWindow,
    state: State<'_, AppState>,
) -> Result<ClientWindowState, String> {
    let result = match action.as_str() {
        "minimize" => window.minimize(),
        "toggle_maximize" => {
            if window.is_maximized().map_err(|error| error.to_string())? {
                window.unmaximize()
            } else {
                window.maximize()
            }
        }
        "close" => window.close(),
        "state" => Ok(()),
        "set_title" => {
            let title = normalized_window_title(value);
            window.set_title(&title)
        }
        "show" => window.show(),
        _ => return Err(format!("unknown client window action: {action}")),
    };
    result.map_err(|error| error.to_string())?;
    if action == "show" {
        state.window_revealed.store(true, Ordering::Release);
    }
    client_window_state(&window)
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
    let (device_preferences, remembered_devices) = run_blocking(move || {
        Ok((
            load_device_preferences(&cache)?,
            cache.remembered_devices()?,
        ))
    })
    .await?;
    let online_devices = online_remembered_devices(
        remembered_devices
            .iter()
            .map(|device| device.endpoint.clone())
            .collect(),
    )
    .await;
    let remembered_devices = remembered_devices
        .into_iter()
        .map(|device| {
            let online = online_devices.contains(&device.endpoint);
            RememberedDeviceStatus::from_device(device, online)
        })
        .collect();
    let local_device = discover_local_device().await;
    Ok(ClientBootstrap {
        endpoint,
        device_preferences,
        remembered_devices,
        local_device,
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
async fn remember_device(
    endpoint: String,
    password: String,
    state: State<'_, AppState>,
) -> Result<RememberedDeviceStatus, String> {
    let endpoint = normalize_endpoint(&endpoint)?;
    if password.len() > MAX_REMEMBERED_PASSWORD_BYTES {
        return Err("密码过长".into());
    }
    let cache = state.cache.clone();
    let device = run_blocking(move || cache.remember_device(&endpoint, &password)).await?;
    Ok(RememberedDeviceStatus::from_device(device, true))
}

#[tauri::command]
async fn forget_device(endpoint: String, state: State<'_, AppState>) -> Result<(), String> {
    let endpoint = normalize_endpoint(&endpoint)?;
    let cache = state.cache.clone();
    run_blocking(move || cache.forget_device(&endpoint)).await
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
    Ok(AppState {
        cache,
        gateway,
        window_revealed: AtomicBool::new(false),
    })
}

fn database_path(data_directory: PathBuf) -> PathBuf {
    data_directory.join("me-client.sqlite3")
}

#[cfg(target_os = "macos")]
const NEW_CLIENT_WINDOW_LABEL: &str = "新建窗口";

#[cfg(target_os = "macos")]
extern "C-unwind" fn application_dock_menu(
    delegate: &AnyObject,
    _selector: Sel,
    application: &NSApplication,
) -> *mut NSMenu {
    let marker = MainThreadMarker::from(application);
    let menu = NSMenu::new(marker);
    let item = NSMenuItem::new(marker);
    item.setTitle(&NSString::from_str(NEW_CLIENT_WINDOW_LABEL));
    unsafe {
        item.setAction(Some(sel!(newMeClientWindow:)));
        item.setTarget(Some(delegate));
    }
    menu.addItem(&item);
    Retained::autorelease_return(menu)
}

#[cfg(target_os = "macos")]
extern "C-unwind" fn new_me_client_window(
    _delegate: &AnyObject,
    _selector: Sel,
    _sender: &AnyObject,
) {
    if let Err(error) = spawn_client_instance() {
        log::error!("failed to open another ME Client instance: {error}");
    }
}

#[cfg(target_os = "macos")]
fn install_macos_dock_menu() -> Result<(), io::Error> {
    let marker = MainThreadMarker::new()
        .ok_or_else(|| io::Error::other("macOS Dock menu must be installed on the main thread"))?;
    let application = NSApplication::sharedApplication(marker);
    let delegate = application
        .delegate()
        .ok_or_else(|| io::Error::other("macOS application delegate is unavailable"))?;

    // Tauri owns the application delegate, so extend its class instead of replacing it.
    let delegate_pointer = Retained::as_ptr(&delegate).cast::<AnyObject>();
    let delegate_class = unsafe { ffi::object_getClass(delegate_pointer) as *mut AnyClass };
    if delegate_class.is_null() {
        return Err(io::Error::other(
            "macOS application delegate class is unavailable",
        ));
    }

    let new_window_implementation: Imp = unsafe {
        std::mem::transmute::<extern "C-unwind" fn(&AnyObject, Sel, &AnyObject), Imp>(
            new_me_client_window,
        )
    };
    let dock_menu_implementation: Imp = unsafe {
        std::mem::transmute::<
            extern "C-unwind" fn(&AnyObject, Sel, &NSApplication) -> *mut NSMenu,
            Imp,
        >(application_dock_menu)
    };
    unsafe {
        if !ffi::class_addMethod(
            delegate_class,
            sel!(newMeClientWindow:),
            new_window_implementation,
            c"v@:@".as_ptr(),
        )
        .as_bool()
        {
            return Err(io::Error::other(
                "failed to install the macOS New Window action",
            ));
        }
        if !ffi::class_addMethod(
            delegate_class,
            sel!(applicationDockMenu:),
            dock_menu_implementation,
            c"@@:@".as_ptr(),
        )
        .as_bool()
        {
            return Err(io::Error::other("failed to install the macOS Dock menu"));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn restore_client_window(app: &AppHandle) -> tauri::Result<()> {
    if !app
        .state::<AppState>()
        .window_revealed
        .load(Ordering::Acquire)
    {
        return Ok(());
    }
    app.show()?;
    if let Some(window) = app.get_webview_window("main") {
        window.unminimize()?;
        window.show()?;
        window.set_focus()?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn spawn_client_instance() -> Result<(), io::Error> {
    let executable = env::current_exe()?;
    let mut child = Command::new(executable)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .setup(|app| {
            app.manage(app_state(app)?);
            #[cfg(target_os = "macos")]
            install_macos_dock_menu()?;
            if let Some(window) = app.get_webview_window("main") {
                window.hide()?;
                client_window_state(&window).map_err(io::Error::other)?;
            }
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
            client_window_action,
            client_bootstrap,
            configure_target,
            set_device_preference,
            remember_device,
            forget_device,
            gateway_request,
            cache_load_metadata,
            cache_load_chunk,
            cache_list,
            cache_save_batch,
            cache_remove,
            download_file,
        ])
        .build(tauri::generate_context!())
        .expect("error while building ME Client");
    app.run(|_app_handle, _event| {
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = _event {
            if let Err(error) = restore_client_window(_app_handle) {
                log::error!("failed to restore the ME Client window: {error}");
            }
        }
    });
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
