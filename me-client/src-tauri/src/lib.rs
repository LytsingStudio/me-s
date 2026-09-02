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
use std::ffi::c_void;

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
    let floating = !state.maximized && !state.fullscreen;
    let corner_radius = if floating { 18.0 } else { 0.0 };
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
        let _: () = msg_send![ns_window, setHasShadow: floating];
        let _: () = msg_send![ns_window, invalidateShadow];
    }
    Ok(())
}

#[cfg(target_os = "windows")]
type WindowsHwnd = *mut c_void;

#[cfg(target_os = "windows")]
type WindowsSubclassProc =
    Option<unsafe extern "system" fn(WindowsHwnd, u32, usize, isize, usize, usize) -> isize>;

#[cfg(target_os = "windows")]
#[repr(C)]
#[derive(Default)]
struct WindowRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[cfg(target_os = "windows")]
const WINDOWS_CORNER_RADIUS_CSS_PX: i32 = 18;
#[cfg(target_os = "windows")]
const WINDOWS_RESIZE_BORDER_CSS_PX: i32 = 8;
#[cfg(target_os = "windows")]
const WINDOWS_SHADOW_MARGIN_CSS_PX: i32 = 48;
#[cfg(target_os = "windows")]
const WINDOWS_MAIN_SUBCLASS_ID: usize = 0x4d45_4d41;
#[cfg(target_os = "windows")]
const WINDOWS_SHADOW_SUBCLASS_ID: usize = 0x4d45_5348;
#[cfg(target_os = "windows")]
const WINDOWS_SHADOW_LABEL: &str = "window-shadow";
#[cfg(target_os = "windows")]
const WM_NCHITTEST: u32 = 0x0084;
#[cfg(target_os = "windows")]
const HTNOWHERE: isize = 0;
#[cfg(target_os = "windows")]
const HTCLIENT: isize = 1;
#[cfg(target_os = "windows")]
const HTLEFT: isize = 10;
#[cfg(target_os = "windows")]
const HTRIGHT: isize = 11;
#[cfg(target_os = "windows")]
const HTTOP: isize = 12;
#[cfg(target_os = "windows")]
const HTTOPLEFT: isize = 13;
#[cfg(target_os = "windows")]
const HTTOPRIGHT: isize = 14;
#[cfg(target_os = "windows")]
const HTBOTTOM: isize = 15;
#[cfg(target_os = "windows")]
const HTBOTTOMLEFT: isize = 16;
#[cfg(target_os = "windows")]
const HTBOTTOMRIGHT: isize = 17;
#[cfg(target_os = "windows")]
const HTTRANSPARENT: isize = -1;
#[cfg(target_os = "windows")]
const GWL_EXSTYLE: i32 = -20;
#[cfg(target_os = "windows")]
const WS_EX_TRANSPARENT: isize = 0x0000_0020;
#[cfg(target_os = "windows")]
const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
#[cfg(target_os = "windows")]
const WS_EX_NOREDIRECTIONBITMAP: isize = 0x0020_0000;
#[cfg(target_os = "windows")]
const WS_EX_NOACTIVATE: isize = 0x0800_0000;
#[cfg(target_os = "windows")]
const SWP_NOSIZE: u32 = 0x0001;
#[cfg(target_os = "windows")]
const SWP_NOMOVE: u32 = 0x0002;
#[cfg(target_os = "windows")]
const SWP_NOZORDER: u32 = 0x0004;
#[cfg(target_os = "windows")]
const SWP_NOACTIVATE: u32 = 0x0010;
#[cfg(target_os = "windows")]
const SWP_FRAMECHANGED: u32 = 0x0020;
#[cfg(target_os = "windows")]
const SWP_SHOWWINDOW: u32 = 0x0040;
#[cfg(target_os = "windows")]
const SW_HIDE: i32 = 0;

#[cfg(target_os = "windows")]
static WINDOWS_WINDOW_SQUARE: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "windows")]
#[link(name = "user32")]
unsafe extern "system" {
    fn GetWindowRect(hwnd: WindowsHwnd, rect: *mut WindowRect) -> i32;
    fn GetDpiForWindow(hwnd: WindowsHwnd) -> u32;
    fn GetWindowLongPtrW(hwnd: WindowsHwnd, index: i32) -> isize;
    fn SetWindowLongPtrW(hwnd: WindowsHwnd, index: i32, value: isize) -> isize;
    fn SetWindowPos(
        hwnd: WindowsHwnd,
        insert_after: WindowsHwnd,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        flags: u32,
    ) -> i32;
    fn ShowWindow(hwnd: WindowsHwnd, command: i32) -> i32;
    fn IsIconic(hwnd: WindowsHwnd) -> i32;
}

#[cfg(target_os = "windows")]
#[link(name = "comctl32")]
unsafe extern "system" {
    fn SetWindowSubclass(
        hwnd: WindowsHwnd,
        proc: WindowsSubclassProc,
        id: usize,
        reference_data: usize,
    ) -> i32;
    fn DefSubclassProc(hwnd: WindowsHwnd, msg: u32, wparam: usize, lparam: isize) -> isize;
}

#[cfg(target_os = "windows")]
fn scaled_windows_px(logical: i32, dpi: u32) -> i32 {
    let dpi = if dpi == 0 { 96 } else { dpi };
    ((logical as i64 * dpi as i64 + 48) / 96) as i32
}

#[cfg(target_os = "windows")]
fn outside_rounded_corner(x: i32, y: i32, width: i32, height: i32, radius: i32) -> bool {
    let center_x = if x < radius {
        radius
    } else if x >= width - radius {
        width - radius
    } else {
        return false;
    };
    let center_y = if y < radius {
        radius
    } else if y >= height - radius {
        height - radius
    } else {
        return false;
    };
    let dx = (x - center_x) as i64;
    let dy = (y - center_y) as i64;
    dx * dx + dy * dy > (radius as i64 * radius as i64)
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn windows_rounded_hit_test(
    hwnd: WindowsHwnd,
    msg: u32,
    wparam: usize,
    lparam: isize,
    _id: usize,
    _reference_data: usize,
) -> isize {
    if msg != WM_NCHITTEST || WINDOWS_WINDOW_SQUARE.load(Ordering::Acquire) {
        return unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
    }

    let mut rect = WindowRect::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        return unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
    }
    let screen_x = (lparam as u16 as i16) as i32;
    let screen_y = ((lparam >> 16) as u16 as i16) as i32;
    let x = screen_x - rect.left;
    let y = screen_y - rect.top;
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if x < 0 || y < 0 || x >= width || y >= height {
        return HTNOWHERE;
    }

    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let radius = scaled_windows_px(WINDOWS_CORNER_RADIUS_CSS_PX, dpi);
    if outside_rounded_corner(x, y, width, height, radius) {
        return HTNOWHERE;
    }

    let border = scaled_windows_px(WINDOWS_RESIZE_BORDER_CSS_PX, dpi);
    let left = x < border;
    let right = x >= width - border;
    let top = y < border;
    let bottom = y >= height - border;
    match (left, right, top, bottom) {
        (true, _, true, _) => HTTOPLEFT,
        (_, true, true, _) => HTTOPRIGHT,
        (true, _, _, true) => HTBOTTOMLEFT,
        (_, true, _, true) => HTBOTTOMRIGHT,
        (true, _, _, _) => HTLEFT,
        (_, true, _, _) => HTRIGHT,
        (_, _, true, _) => HTTOP,
        (_, _, _, true) => HTBOTTOM,
        _ => HTCLIENT,
    }
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn windows_shadow_hit_test(
    hwnd: WindowsHwnd,
    msg: u32,
    wparam: usize,
    lparam: isize,
    _id: usize,
    _reference_data: usize,
) -> isize {
    if msg == WM_NCHITTEST {
        HTTRANSPARENT
    } else {
        unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
    }
}

#[cfg(target_os = "windows")]
fn install_windows_subclass(
    hwnd: WindowsHwnd,
    proc: WindowsSubclassProc,
    id: usize,
    name: &str,
) -> Result<(), String> {
    if unsafe { SetWindowSubclass(hwnd, proc, id, 0) } == 0 {
        return Err(format!("{name} SetWindowSubclass failed"));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn verify_windows_no_redirection(hwnd: WindowsHwnd, name: &str) -> Result<(), String> {
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    if style & WS_EX_NOREDIRECTIONBITMAP == 0 {
        return Err(format!(
            "{name} was not created with WS_EX_NOREDIRECTIONBITMAP"
        ));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn configure_windows_shadow(hwnd: WindowsHwnd) -> Result<(), String> {
    let required = WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT;
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style | required);
    }
    let actual = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    if actual & required != required {
        return Err("shadow extended styles were not applied".into());
    }
    let updated = unsafe {
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        )
    };
    if updated == 0 {
        return Err("shadow SetWindowPos failed while applying styles".into());
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn create_windows_shadow(app: &tauri::App) -> Result<(), String> {
    let shadow = tauri::WebviewWindowBuilder::new(
        app,
        WINDOWS_SHADOW_LABEL,
        tauri::WebviewUrl::App("window-shadow.html".into()),
    )
    .title("ME Client Shadow")
    .inner_size(1.0, 1.0)
    .resizable(false)
    .decorations(false)
    .transparent(true)
    .shadow(false)
    .visible(false)
    .skip_taskbar(true)
    .focusable(false)
    .devtools(false)
    .build()
    .map_err(|error| error.to_string())?;
    let hwnd = shadow.hwnd().map_err(|error| error.to_string())?.0;
    verify_windows_no_redirection(hwnd, "shadow window")?;
    configure_windows_shadow(hwnd)?;
    install_windows_subclass(
        hwnd,
        Some(windows_shadow_hit_test),
        WINDOWS_SHADOW_SUBCLASS_ID,
        "shadow window",
    )
}

#[cfg(target_os = "windows")]
fn initialize_windows_window(app: &tauri::App, main: &WebviewWindow) -> Result<(), String> {
    let hwnd = main.hwnd().map_err(|error| error.to_string())?.0;
    verify_windows_no_redirection(hwnd, "main window")?;
    install_windows_subclass(
        hwnd,
        Some(windows_rounded_hit_test),
        WINDOWS_MAIN_SUBCLASS_ID,
        "main window",
    )?;
    create_windows_shadow(app)
}

#[cfg(target_os = "windows")]
fn sync_windows_shadow(window: &WebviewWindow, state: &ClientWindowState) -> Result<(), String> {
    let shadow = window
        .app_handle()
        .get_webview_window(WINDOWS_SHADOW_LABEL)
        .ok_or_else(|| "shadow window is unavailable".to_owned())?;
    let main_hwnd = window.hwnd().map_err(|error| error.to_string())?.0;
    let shadow_hwnd = shadow.hwnd().map_err(|error| error.to_string())?.0;
    let revealed = window
        .app_handle()
        .state::<AppState>()
        .window_revealed
        .load(Ordering::Acquire);
    let floating =
        revealed && !state.maximized && !state.fullscreen && unsafe { IsIconic(main_hwnd) } == 0;
    if !floating {
        unsafe {
            ShowWindow(shadow_hwnd, SW_HIDE);
        }
        return Ok(());
    }

    let mut rect = WindowRect::default();
    if unsafe { GetWindowRect(main_hwnd, &mut rect) } == 0 {
        return Err("GetWindowRect failed while synchronizing shadow".into());
    }
    let margin = scaled_windows_px(WINDOWS_SHADOW_MARGIN_CSS_PX, unsafe {
        GetDpiForWindow(main_hwnd)
    });
    let positioned = unsafe {
        SetWindowPos(
            shadow_hwnd,
            main_hwnd,
            rect.left - margin,
            rect.top - margin,
            rect.right - rect.left + margin * 2,
            rect.bottom - rect.top + margin * 2,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    };
    if positioned == 0 {
        return Err("SetWindowPos failed while synchronizing shadow".into());
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn apply_platform_window_shape(
    window: &WebviewWindow,
    state: &ClientWindowState,
) -> Result<(), String> {
    let floating = !state.maximized && !state.fullscreen;
    WINDOWS_WINDOW_SQUARE.store(!floating, Ordering::Release);
    sync_windows_shadow(window, state)
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

#[cfg(target_os = "macos")]
fn close_client_window(window: &WebviewWindow) -> tauri::Result<()> {
    window.hide()
}

#[cfg(target_os = "windows")]
fn close_client_window(window: &WebviewWindow) -> tauri::Result<()> {
    if let Some(shadow) = window.app_handle().get_webview_window(WINDOWS_SHADOW_LABEL) {
        let _ = shadow.close();
    }
    window.close()
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn close_client_window(window: &WebviewWindow) -> tauri::Result<()> {
    window.close()
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
        "close" => close_client_window(&window),
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

#[cfg(target_os = "windows")]
fn handle_windows_run_event(app: &AppHandle, run_event: &tauri::RunEvent) {
    let tauri::RunEvent::WindowEvent { label, event, .. } = run_event else {
        return;
    };
    if label != "main" {
        return;
    }
    if matches!(event, tauri::WindowEvent::Destroyed) {
        if let Some(shadow) = app.get_webview_window(WINDOWS_SHADOW_LABEL) {
            let _ = shadow.close();
        }
        return;
    }
    if matches!(
        event,
        tauri::WindowEvent::Moved(_)
            | tauri::WindowEvent::Resized(_)
            | tauri::WindowEvent::ScaleFactorChanged { .. }
            | tauri::WindowEvent::Focused(_)
    ) {
        if let Some(window) = app.get_webview_window("main") {
            if let Err(error) = client_window_state(&window) {
                log::error!("failed to synchronize the Windows client window: {error}");
            }
        }
    }
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
                #[cfg(target_os = "windows")]
                initialize_windows_window(app, &window).map_err(io::Error::other)?;
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
        #[cfg(target_os = "windows")]
        handle_windows_run_event(_app_handle, &_event);
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
