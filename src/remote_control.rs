use std::{
    collections::BTreeSet,
    fmt,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use fs2::FileExt;
use image::{ExtendedColorType, RgbaImage, codecs::jpeg::JpegEncoder, imageops::FilterType};
use serde::{Deserialize, Serialize};

use crate::{config::config_home, managed_protocol::random_hex_secret};

#[cfg(target_os = "macos")]
#[path = "remote_control/macos.rs"]
mod macos;

#[cfg(target_os = "windows")]
#[path = "remote_control/windows.rs"]
mod windows;

pub const REMOTE_CONTROL_PATH_PREFIX: &str = "/api/remote-control/";
pub const MAX_REMOTE_CONTROL_BODY_BYTES: usize = 256 * 1024;

const CONTROLLER_TIMEOUT: Duration = Duration::from_secs(15);
const TARGET_JPEG_BYTES: usize = 192 * 1024;
const MAX_INPUT_EVENTS: usize = 128;
const MAX_TEXT_BYTES: usize = 32 * 1024;
const MAX_WHEEL_DELTA: i32 = 10_000;
const FPS_VALUES: [u8; 4] = [1, 3, 5, 10];
const SCALE_VALUES: [u8; 4] = [25, 50, 75, 100];
const JPEG_QUALITIES: [u8; 4] = [50, 40, 30, 20];

pub type RemoteResult<T> = std::result::Result<T, RemoteControlError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteControlError {
    code: &'static str,
    message: String,
    status: u16,
}

impl RemoteControlError {
    fn new(code: &'static str, message: impl Into<String>, status: u16) -> Self {
        Self {
            code,
            message: message.into(),
            status,
        }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    pub(crate) fn capture(message: impl Into<String>) -> Self {
        Self::new("remote_capture_failed", message, 500)
    }

    pub(crate) fn input(message: impl Into<String>) -> Self {
        Self::new("remote_input_failed", message, 500)
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn screen_permission() -> Self {
        Self::new(
            "screen_permission_required",
            "macOS Screen Recording permission is required for remote control",
            403,
        )
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn accessibility_permission() -> Self {
        Self::new(
            "accessibility_permission_required",
            "macOS Accessibility permission is required for remote control input",
            403,
        )
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn interactive_desktop(message: impl Into<String>) -> Self {
        Self::new("interactive_desktop_required", message, 409)
    }
}

impl fmt::Display for RemoteControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RemoteControlError {}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScreenGeometry {
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub logical_x: f64,
    pub logical_y: f64,
    pub logical_width: f64,
    pub logical_height: f64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum RemoteMouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RemoteInputEvent {
    MouseMove { x: u32, y: u32 },
    MouseDown { button: RemoteMouseButton },
    MouseUp { button: RemoteMouseButton },
    MouseWheel { delta_x: i32, delta_y: i32 },
    KeyDown { code: String },
    KeyUp { code: String },
    Text { text: String },
}

#[derive(Clone, Debug)]
enum ValidatedInputEvent {
    MouseMove { x: u32, y: u32 },
    MouseDown { button: RemoteMouseButton },
    MouseUp { button: RemoteMouseButton },
    MouseWheel { delta_x: i32, delta_y: i32 },
    KeyDown { key: String },
    KeyUp { key: String },
    Text { text: String },
}

pub(crate) trait NativeRemoteControlBackend: Send {
    fn preflight_control(&mut self) -> RemoteResult<ScreenGeometry>;
    fn capture(&mut self) -> RemoteResult<(ScreenGeometry, RgbaImage)>;
    fn mouse_move(&mut self, x: u32, y: u32, held: &[RemoteMouseButton]) -> RemoteResult<()>;
    fn mouse_down(&mut self, button: RemoteMouseButton) -> RemoteResult<()>;
    fn mouse_up(&mut self, button: RemoteMouseButton) -> RemoteResult<()>;
    fn mouse_wheel(&mut self, delta_x: i32, delta_y: i32) -> RemoteResult<()>;
    fn key_down(&mut self, key: &str) -> RemoteResult<()>;
    fn key_up(&mut self, key: &str) -> RemoteResult<()>;
    fn text(&mut self, text: &str) -> RemoteResult<()>;
}

#[derive(Clone, Debug)]
pub struct RemoteFrame {
    pub sequence: u64,
    pub screen_width: u32,
    pub screen_height: u32,
    pub frame_width: u32,
    pub frame_height: u32,
    pub jpeg: Arc<Vec<u8>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RemoteControlStatus {
    pub ok: bool,
    pub supported: bool,
    pub active: bool,
    pub owned: bool,
    pub fps: Option<u8>,
    pub scale: Option<u8>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RemoteControlStart {
    pub ok: bool,
    pub controller_token: String,
    pub fps: u8,
    pub scale: u8,
}

#[derive(Clone, Debug, Serialize)]
pub struct RemoteControlOperation {
    pub ok: bool,
    pub active: bool,
    pub released_inputs: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cleanup_errors: Vec<String>,
}

#[derive(Debug)]
struct Controller {
    token: String,
    fps: u8,
    scale: u8,
    last_seen: Instant,
    last_capture_at: Option<Instant>,
    latest_frame: Option<RemoteFrame>,
    pressed_keys: BTreeSet<String>,
    pressed_buttons: BTreeSet<RemoteMouseButton>,
    _lease: RemoteControlLease,
}

#[derive(Default)]
struct RuntimeState {
    controller: Option<Controller>,
    capture_in_progress: bool,
    next_sequence: u64,
}

pub struct RemoteControlRuntime {
    backend: Mutex<Option<Box<dyn NativeRemoteControlBackend>>>,
    state: Mutex<RuntimeState>,
    lease_directory: PathBuf,
    shutting_down: AtomicBool,
}

impl RemoteControlRuntime {
    pub fn new() -> crate::Result<Self> {
        Ok(Self {
            backend: Mutex::new(system_backend()),
            state: Mutex::new(RuntimeState::default()),
            lease_directory: config_home()?.join("remote-control"),
            shutting_down: AtomicBool::new(false),
        })
    }

    pub fn status(&self, token: Option<&str>) -> RemoteResult<RemoteControlStatus> {
        self.expire_if_stale();
        let supported = self
            .backend
            .lock()
            .map_err(|_| internal_lock_error())?
            .is_some();
        let state = self.state.lock().map_err(|_| internal_lock_error())?;
        let controller = state.controller.as_ref();
        Ok(RemoteControlStatus {
            ok: true,
            supported,
            active: controller.is_some(),
            owned: controller
                .zip(token)
                .is_some_and(|(controller, token)| token_matches(&controller.token, token)),
            fps: controller.map(|controller| controller.fps),
            scale: controller.map(|controller| controller.scale),
        })
    }

    pub fn start(&self, fps: u8, scale: u8) -> RemoteResult<RemoteControlStart> {
        validate_settings(fps, scale)?;
        self.ensure_running()?;
        self.expire_if_stale();
        {
            let state = self.state.lock().map_err(|_| internal_lock_error())?;
            if state.controller.is_some() {
                return Err(remote_busy());
            }
        }
        if self
            .backend
            .lock()
            .map_err(|_| internal_lock_error())?
            .is_none()
        {
            return Err(unsupported_platform());
        }
        let token = random_hex_secret(32).map_err(|error| {
            RemoteControlError::new(
                "remote_control_unavailable",
                format!("cannot generate a remote-control identity: {error}"),
                500,
            )
        })?;
        let lease = RemoteControlLease::acquire_at(&self.lease_directory)?;
        {
            let mut backend = self.backend.lock().map_err(|_| internal_lock_error())?;
            backend
                .as_mut()
                .ok_or_else(unsupported_platform)?
                .preflight_control()?;
        }
        let mut state = self.state.lock().map_err(|_| internal_lock_error())?;
        if state.controller.is_some() {
            return Err(remote_busy());
        }
        state.controller = Some(Controller {
            token: token.clone(),
            fps,
            scale,
            last_seen: Instant::now(),
            last_capture_at: None,
            latest_frame: None,
            pressed_keys: BTreeSet::new(),
            pressed_buttons: BTreeSet::new(),
            _lease: lease,
        });
        Ok(RemoteControlStart {
            ok: true,
            controller_token: token,
            fps,
            scale,
        })
    }

    pub fn stop(&self, token: &str) -> RemoteResult<RemoteControlOperation> {
        self.expire_if_stale();
        let controller = self.take_owned_controller(token)?;
        Ok(self.finish_controller(controller))
    }

    pub fn keepalive(&self, token: &str) -> RemoteResult<RemoteControlOperation> {
        self.expire_if_stale();
        let mut state = self.state.lock().map_err(|_| internal_lock_error())?;
        let controller = owned_controller_mut(&mut state, token)?;
        controller.last_seen = Instant::now();
        Ok(RemoteControlOperation {
            ok: true,
            active: true,
            released_inputs: 0,
            cleanup_errors: Vec::new(),
        })
    }

    pub fn settings(
        &self,
        token: &str,
        fps: u8,
        scale: u8,
    ) -> RemoteResult<RemoteControlOperation> {
        validate_settings(fps, scale)?;
        self.expire_if_stale();
        let mut state = self.state.lock().map_err(|_| internal_lock_error())?;
        let controller = owned_controller_mut(&mut state, token)?;
        controller.fps = fps;
        if controller.scale != scale {
            controller.scale = scale;
            controller.latest_frame = None;
        }
        controller.last_capture_at = None;
        controller.last_seen = Instant::now();
        Ok(RemoteControlOperation {
            ok: true,
            active: true,
            released_inputs: 0,
            cleanup_errors: Vec::new(),
        })
    }

    pub fn frame(
        &self,
        token: &str,
        after_sequence: Option<u64>,
    ) -> RemoteResult<Option<RemoteFrame>> {
        self.ensure_running()?;
        self.expire_if_stale();
        let scale = {
            let mut state = self.state.lock().map_err(|_| internal_lock_error())?;
            let capture_in_progress = state.capture_in_progress;
            let controller = owned_controller_mut(&mut state, token)?;
            controller.last_seen = Instant::now();
            if let Some(frame) = controller.latest_frame.as_ref()
                && after_sequence.is_none_or(|after| frame.sequence > after)
            {
                return Ok(Some(frame.clone()));
            }
            let interval = Duration::from_secs_f64(1.0 / f64::from(controller.fps));
            if capture_in_progress
                || controller
                    .last_capture_at
                    .is_some_and(|captured| captured.elapsed() < interval)
            {
                return Ok(None);
            }
            controller.last_capture_at = Some(Instant::now());
            let scale = controller.scale;
            state.capture_in_progress = true;
            scale
        };

        let capture = self.capture_encoded(scale);
        let mut state = self.state.lock().map_err(|_| internal_lock_error())?;
        state.capture_in_progress = false;
        let encoded = capture?;
        let owned = state
            .controller
            .as_ref()
            .is_some_and(|controller| token_matches(&controller.token, token));
        if !owned {
            return Err(not_owned());
        }
        state.next_sequence = state.next_sequence.saturating_add(1);
        let frame = RemoteFrame {
            sequence: state.next_sequence,
            screen_width: encoded.screen_width,
            screen_height: encoded.screen_height,
            frame_width: encoded.frame_width,
            frame_height: encoded.frame_height,
            jpeg: Arc::new(encoded.jpeg),
        };
        if let Some(controller) = state.controller.as_mut() {
            controller.latest_frame = Some(frame.clone());
        }
        Ok(Some(frame))
    }

    pub fn screenshot(&self, scale: u8) -> RemoteResult<Option<RemoteFrame>> {
        validate_scale(scale)?;
        self.ensure_running()?;
        {
            let mut state = self.state.lock().map_err(|_| internal_lock_error())?;
            if state.capture_in_progress {
                return Ok(None);
            }
            state.capture_in_progress = true;
        }
        let capture = self.capture_encoded(scale);
        let mut state = self.state.lock().map_err(|_| internal_lock_error())?;
        state.capture_in_progress = false;
        let encoded = capture?;
        state.next_sequence = state.next_sequence.saturating_add(1);
        Ok(Some(RemoteFrame {
            sequence: state.next_sequence,
            screen_width: encoded.screen_width,
            screen_height: encoded.screen_height,
            frame_width: encoded.frame_width,
            frame_height: encoded.frame_height,
            jpeg: Arc::new(encoded.jpeg),
        }))
    }

    pub fn input(
        &self,
        token: &str,
        events: &[RemoteInputEvent],
    ) -> RemoteResult<RemoteControlOperation> {
        self.ensure_running()?;
        self.expire_if_stale();
        let mut state = self.state.lock().map_err(|_| internal_lock_error())?;
        let controller = owned_controller_mut(&mut state, token)?;
        controller.last_seen = Instant::now();
        let mut backend = self.backend.lock().map_err(|_| internal_lock_error())?;
        let backend = backend.as_mut().ok_or_else(unsupported_platform)?;
        let geometry = backend.preflight_control()?;
        let events = validate_input_events(events, geometry)?;
        for event in events {
            match event {
                ValidatedInputEvent::MouseMove { x, y } => {
                    let held = controller
                        .pressed_buttons
                        .iter()
                        .copied()
                        .collect::<Vec<_>>();
                    backend.mouse_move(x, y, &held)?;
                }
                ValidatedInputEvent::MouseDown { button } => {
                    backend.mouse_down(button)?;
                    controller.pressed_buttons.insert(button);
                }
                ValidatedInputEvent::MouseUp { button } => {
                    backend.mouse_up(button)?;
                    controller.pressed_buttons.remove(&button);
                }
                ValidatedInputEvent::MouseWheel { delta_x, delta_y } => {
                    backend.mouse_wheel(delta_x, delta_y)?;
                }
                ValidatedInputEvent::KeyDown { key } => {
                    backend.key_down(&key)?;
                    controller.pressed_keys.insert(key);
                }
                ValidatedInputEvent::KeyUp { key } => {
                    backend.key_up(&key)?;
                    controller.pressed_keys.remove(&key);
                }
                ValidatedInputEvent::Text { text } => backend.text(&text)?,
            }
        }
        Ok(RemoteControlOperation {
            ok: true,
            active: true,
            released_inputs: 0,
            cleanup_errors: Vec::new(),
        })
    }

    pub fn release_inputs(&self, token: &str) -> RemoteResult<RemoteControlOperation> {
        self.expire_if_stale();
        let mut state = self.state.lock().map_err(|_| internal_lock_error())?;
        let controller = owned_controller_mut(&mut state, token)?;
        controller.last_seen = Instant::now();
        let keys = controller.pressed_keys.clone();
        let buttons = controller.pressed_buttons.clone();
        let (released_inputs, cleanup_errors) = self.release_tracked(keys, buttons);
        if cleanup_errors.is_empty() {
            controller.pressed_keys.clear();
            controller.pressed_buttons.clear();
        }
        Ok(RemoteControlOperation {
            ok: cleanup_errors.is_empty(),
            active: true,
            released_inputs,
            cleanup_errors,
        })
    }

    pub fn expire_if_stale(&self) {
        let controller = self.state.lock().ok().and_then(|mut state| {
            let expired = state
                .controller
                .as_ref()
                .is_some_and(|controller| controller.last_seen.elapsed() >= CONTROLLER_TIMEOUT);
            expired.then(|| state.controller.take().expect("expired controller exists"))
        });
        if let Some(controller) = controller {
            let _ = self.finish_controller(controller);
        }
    }

    pub fn shutdown(&self) {
        if self.shutting_down.swap(true, Ordering::AcqRel) {
            return;
        }
        let controller = self
            .state
            .lock()
            .ok()
            .and_then(|mut state| state.controller.take());
        if let Some(controller) = controller {
            let _ = self.finish_controller(controller);
        }
    }

    fn capture_encoded(&self, scale: u8) -> RemoteResult<EncodedFrame> {
        let (geometry, image) = self
            .backend
            .lock()
            .map_err(|_| internal_lock_error())?
            .as_mut()
            .ok_or_else(unsupported_platform)?
            .capture()?;
        encode_frame(geometry, image, scale)
    }

    fn ensure_running(&self) -> RemoteResult<()> {
        if self.shutting_down.load(Ordering::Acquire) {
            Err(RemoteControlError::new(
                "remote_control_stopping",
                "remote control is stopping",
                503,
            ))
        } else {
            Ok(())
        }
    }

    fn take_owned_controller(&self, token: &str) -> RemoteResult<Controller> {
        let mut state = self.state.lock().map_err(|_| internal_lock_error())?;
        if !state
            .controller
            .as_ref()
            .is_some_and(|controller| token_matches(&controller.token, token))
        {
            return Err(not_owned());
        }
        Ok(state.controller.take().expect("owned controller exists"))
    }

    fn finish_controller(&self, controller: Controller) -> RemoteControlOperation {
        let (released_inputs, cleanup_errors) =
            self.release_tracked(controller.pressed_keys, controller.pressed_buttons);
        RemoteControlOperation {
            ok: cleanup_errors.is_empty(),
            active: false,
            released_inputs,
            cleanup_errors,
        }
    }

    fn release_tracked(
        &self,
        keys: BTreeSet<String>,
        buttons: BTreeSet<RemoteMouseButton>,
    ) -> (usize, Vec<String>) {
        let mut released = 0;
        let mut errors = Vec::new();
        let Ok(mut backend) = self.backend.lock() else {
            return (0, vec!["remote-control backend lock is unavailable".into()]);
        };
        let Some(backend) = backend.as_mut() else {
            return (0, Vec::new());
        };
        for key in keys.into_iter().rev() {
            match backend.key_up(&key) {
                Ok(()) => released += 1,
                Err(error) => errors.push(format!("key:{key}: {error}")),
            }
        }
        for button in buttons.into_iter().rev() {
            match backend.mouse_up(button) {
                Ok(()) => released += 1,
                Err(error) => errors.push(format!("mouse:{button:?}: {error}")),
            }
        }
        (released, errors)
    }
}

impl Drop for RemoteControlRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Debug)]
struct EncodedFrame {
    screen_width: u32,
    screen_height: u32,
    frame_width: u32,
    frame_height: u32,
    jpeg: Vec<u8>,
}

fn encode_frame(
    geometry: ScreenGeometry,
    image: RgbaImage,
    scale: u8,
) -> RemoteResult<EncodedFrame> {
    validate_scale(scale)?;
    if image.width() != geometry.pixel_width || image.height() != geometry.pixel_height {
        return Err(RemoteControlError::capture(format!(
            "remote screenshot dimensions {}x{} do not match the primary display {}x{}",
            image.width(),
            image.height(),
            geometry.pixel_width,
            geometry.pixel_height
        )));
    }
    let frame_width = scaled_extent(geometry.pixel_width, scale);
    let frame_height = scaled_extent(geometry.pixel_height, scale);
    let image = if frame_width == image.width() && frame_height == image.height() {
        image
    } else {
        image::imageops::resize(&image, frame_width, frame_height, FilterType::Triangle)
    };
    let rgb = image::DynamicImage::ImageRgba8(image).to_rgb8();
    let mut smallest = Vec::new();
    for quality in JPEG_QUALITIES {
        let mut encoded = Vec::new();
        JpegEncoder::new_with_quality(&mut encoded, quality)
            .encode(
                rgb.as_raw(),
                frame_width,
                frame_height,
                ExtendedColorType::Rgb8,
            )
            .map_err(|error| {
                RemoteControlError::capture(format!("cannot encode remote JPEG frame: {error}"))
            })?;
        if smallest.is_empty() || encoded.len() < smallest.len() {
            smallest = encoded;
        }
        if smallest.len() <= TARGET_JPEG_BYTES {
            break;
        }
    }
    if smallest.is_empty() {
        return Err(RemoteControlError::capture(
            "remote JPEG encoder returned an empty frame",
        ));
    }
    Ok(EncodedFrame {
        screen_width: geometry.pixel_width,
        screen_height: geometry.pixel_height,
        frame_width,
        frame_height,
        jpeg: smallest,
    })
}

fn scaled_extent(value: u32, scale: u8) -> u32 {
    ((u64::from(value) * u64::from(scale) + 50) / 100)
        .max(1)
        .min(u64::from(u32::MAX)) as u32
}

fn validate_settings(fps: u8, scale: u8) -> RemoteResult<()> {
    if !FPS_VALUES.contains(&fps) {
        return Err(RemoteControlError::new(
            "invalid_arguments",
            "remote-control fps must be one of 1, 3, 5, or 10",
            400,
        ));
    }
    validate_scale(scale)
}

fn validate_scale(scale: u8) -> RemoteResult<()> {
    if SCALE_VALUES.contains(&scale) {
        Ok(())
    } else {
        Err(RemoteControlError::new(
            "invalid_arguments",
            "remote-control scale must be one of 25, 50, 75, or 100",
            400,
        ))
    }
}

fn validate_input_events(
    events: &[RemoteInputEvent],
    geometry: ScreenGeometry,
) -> RemoteResult<Vec<ValidatedInputEvent>> {
    if events.is_empty() || events.len() > MAX_INPUT_EVENTS {
        return Err(RemoteControlError::new(
            "invalid_arguments",
            format!("remote-control input must contain 1 to {MAX_INPUT_EVENTS} events"),
            400,
        ));
    }
    events
        .iter()
        .map(|event| match event {
            RemoteInputEvent::MouseMove { x, y } => {
                if *x >= geometry.pixel_width || *y >= geometry.pixel_height {
                    return Err(RemoteControlError::new(
                        "invalid_arguments",
                        format!(
                            "remote mouse coordinate ({x}, {y}) is outside the primary display {}x{}",
                            geometry.pixel_width, geometry.pixel_height
                        ),
                        400,
                    ));
                }
                Ok(ValidatedInputEvent::MouseMove { x: *x, y: *y })
            }
            RemoteInputEvent::MouseDown { button } => {
                Ok(ValidatedInputEvent::MouseDown { button: *button })
            }
            RemoteInputEvent::MouseUp { button } => {
                Ok(ValidatedInputEvent::MouseUp { button: *button })
            }
            RemoteInputEvent::MouseWheel { delta_x, delta_y } => {
                if (*delta_x == 0 && *delta_y == 0)
                    || delta_x.unsigned_abs() > MAX_WHEEL_DELTA as u32
                    || delta_y.unsigned_abs() > MAX_WHEEL_DELTA as u32
                {
                    return Err(RemoteControlError::new(
                        "invalid_arguments",
                        "remote mouse wheel delta is invalid",
                        400,
                    ));
                }
                Ok(ValidatedInputEvent::MouseWheel {
                    delta_x: *delta_x,
                    delta_y: *delta_y,
                })
            }
            RemoteInputEvent::KeyDown { code } => normalize_dom_code(code)
                .map(|key| ValidatedInputEvent::KeyDown { key })
                .ok_or_else(|| {
                    RemoteControlError::new(
                        "invalid_arguments",
                        format!("unsupported remote keyboard code {code:?}"),
                        400,
                    )
                }),
            RemoteInputEvent::KeyUp { code } => normalize_dom_code(code)
                .map(|key| ValidatedInputEvent::KeyUp { key })
                .ok_or_else(|| {
                    RemoteControlError::new(
                        "invalid_arguments",
                        format!("unsupported remote keyboard code {code:?}"),
                        400,
                    )
                }),
            RemoteInputEvent::Text { text } => {
                if text.is_empty() || text.len() > MAX_TEXT_BYTES {
                    return Err(RemoteControlError::new(
                        "invalid_arguments",
                        format!("remote text must contain 1 to {MAX_TEXT_BYTES} UTF-8 bytes"),
                        400,
                    ));
                }
                Ok(ValidatedInputEvent::Text { text: text.clone() })
            }
        })
        .collect()
}

fn normalize_dom_code(code: &str) -> Option<String> {
    if let Some(letter) = code.strip_prefix("Key")
        && letter.len() == 1
        && letter.as_bytes()[0].is_ascii_alphabetic()
    {
        return Some(letter.to_ascii_lowercase());
    }
    if let Some(digit) = code.strip_prefix("Digit")
        && digit.len() == 1
        && digit.as_bytes()[0].is_ascii_digit()
    {
        return Some(digit.to_owned());
    }
    if let Some(number) = code
        .strip_prefix('F')
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|value| (1..=20).contains(value))
    {
        return Some(format!("f{number}"));
    }
    Some(
        match code {
            "Enter" | "NumpadEnter" => "return",
            "Escape" => "escape",
            "Space" => "space",
            "Tab" => "tab",
            "Backspace" => "backspace",
            "Delete" => "delete",
            "ArrowLeft" => "left",
            "ArrowRight" => "right",
            "ArrowUp" => "up",
            "ArrowDown" => "down",
            "Home" => "home",
            "End" => "end",
            "PageUp" => "page_up",
            "PageDown" => "page_down",
            "ShiftLeft" => "shift",
            "ShiftRight" => "right_shift",
            "ControlLeft" => "control",
            "ControlRight" => "right_control",
            "AltLeft" => "option",
            "AltRight" => "right_option",
            "MetaLeft" => "command",
            "MetaRight" => "right_command",
            "CapsLock" => "caps_lock",
            "Minus" => "minus",
            "Equal" => "equal",
            "BracketLeft" => "left_bracket",
            "BracketRight" => "right_bracket",
            "Backslash" => "backslash",
            "Semicolon" => "semicolon",
            "Quote" => "quote",
            "Comma" => "comma",
            "Period" => "period",
            "Slash" => "slash",
            "Backquote" => "grave",
            _ => return None,
        }
        .to_owned(),
    )
}

fn owned_controller_mut<'a>(
    state: &'a mut RuntimeState,
    token: &str,
) -> RemoteResult<&'a mut Controller> {
    state
        .controller
        .as_mut()
        .filter(|controller| token_matches(&controller.token, token))
        .ok_or_else(not_owned)
}

fn token_matches(expected: &str, candidate: &str) -> bool {
    if expected.len() != candidate.len() {
        return false;
    }
    expected
        .as_bytes()
        .iter()
        .zip(candidate.as_bytes())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn unsupported_platform() -> RemoteControlError {
    RemoteControlError::new(
        "remote_control_unsupported",
        "remote control is supported on macOS and Windows",
        501,
    )
}

fn remote_busy() -> RemoteControlError {
    RemoteControlError::new(
        "remote_control_busy",
        "another WebUI currently controls this host desktop",
        409,
    )
}

fn not_owned() -> RemoteControlError {
    RemoteControlError::new(
        "remote_control_not_owned",
        "this WebUI does not own the active remote-control session",
        409,
    )
}

fn internal_lock_error() -> RemoteControlError {
    RemoteControlError::new(
        "remote_control_unavailable",
        "remote-control state is unavailable",
        500,
    )
}

#[derive(Debug)]
struct RemoteControlLease {
    file: File,
}

impl RemoteControlLease {
    fn acquire_at(directory: &Path) -> RemoteResult<Self> {
        create_private_directory(directory).map_err(|error| {
            RemoteControlError::new(
                "remote_control_lock_failed",
                format!("cannot create the remote-control lock directory: {error}"),
                500,
            )
        })?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(directory.join("control.lock"))
            .map_err(|error| {
                RemoteControlError::new(
                    "remote_control_lock_failed",
                    format!("cannot open the remote-control lock: {error}"),
                    500,
                )
            })?;
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Self { file }),
            Err(error) if remote_lock_is_busy(&error) => Err(remote_busy()),
            Err(error) => Err(RemoteControlError::new(
                "remote_control_lock_failed",
                format!("cannot acquire the remote-control lock: {error}"),
                500,
            )),
        }
    }
}

impl Drop for RemoteControlLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn remote_lock_is_busy(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    {
        // LockFileEx reports contention as ERROR_SHARING_VIOLATION or ERROR_LOCK_VIOLATION.
        return matches!(error.raw_os_error(), Some(32 | 33));
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn create_private_directory(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn system_backend() -> Option<Box<dyn NativeRemoteControlBackend>> {
    Some(Box::new(macos::MacRemoteControlBackend::new()))
}

#[cfg(target_os = "windows")]
fn system_backend() -> Option<Box<dyn NativeRemoteControlBackend>> {
    Some(Box::new(windows::WindowsRemoteControlBackend::new()))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn system_backend() -> Option<Box<dyn NativeRemoteControlBackend>> {
    None
}

#[cfg(test)]
mod tests {
    use std::{sync::atomic::AtomicUsize, time::SystemTime};

    use super::*;

    struct FakeBackend {
        geometry: ScreenGeometry,
        calls: Arc<Mutex<Vec<String>>>,
        captures: Arc<AtomicUsize>,
        reject_preflight: bool,
    }

    impl FakeBackend {
        fn new() -> Self {
            Self {
                geometry: ScreenGeometry {
                    pixel_width: 400,
                    pixel_height: 200,
                    logical_x: 0.0,
                    logical_y: 0.0,
                    logical_width: 400.0,
                    logical_height: 200.0,
                },
                calls: Arc::new(Mutex::new(Vec::new())),
                captures: Arc::new(AtomicUsize::new(0)),
                reject_preflight: false,
            }
        }
    }

    impl NativeRemoteControlBackend for FakeBackend {
        fn preflight_control(&mut self) -> RemoteResult<ScreenGeometry> {
            if self.reject_preflight {
                Err(RemoteControlError::input("fake preflight rejected control"))
            } else {
                Ok(self.geometry)
            }
        }

        fn capture(&mut self) -> RemoteResult<(ScreenGeometry, RgbaImage)> {
            self.captures.fetch_add(1, Ordering::Relaxed);
            Ok((
                self.geometry,
                RgbaImage::from_pixel(400, 200, image::Rgba([10, 20, 30, 255])),
            ))
        }

        fn mouse_move(&mut self, x: u32, y: u32, _held: &[RemoteMouseButton]) -> RemoteResult<()> {
            self.calls.lock().unwrap().push(format!("move:{x}:{y}"));
            Ok(())
        }

        fn mouse_down(&mut self, button: RemoteMouseButton) -> RemoteResult<()> {
            self.calls.lock().unwrap().push(format!("down:{button:?}"));
            Ok(())
        }

        fn mouse_up(&mut self, button: RemoteMouseButton) -> RemoteResult<()> {
            self.calls.lock().unwrap().push(format!("up:{button:?}"));
            Ok(())
        }

        fn mouse_wheel(&mut self, delta_x: i32, delta_y: i32) -> RemoteResult<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("wheel:{delta_x}:{delta_y}"));
            Ok(())
        }

        fn key_down(&mut self, key: &str) -> RemoteResult<()> {
            self.calls.lock().unwrap().push(format!("key-down:{key}"));
            Ok(())
        }

        fn key_up(&mut self, key: &str) -> RemoteResult<()> {
            self.calls.lock().unwrap().push(format!("key-up:{key}"));
            Ok(())
        }

        fn text(&mut self, text: &str) -> RemoteResult<()> {
            self.calls.lock().unwrap().push(format!("text:{text}"));
            Ok(())
        }
    }

    fn test_directory(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "me-remote-control-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn runtime_at(directory: PathBuf) -> (RemoteControlRuntime, Arc<Mutex<Vec<String>>>) {
        let backend = FakeBackend::new();
        let calls = Arc::clone(&backend.calls);
        (
            RemoteControlRuntime {
                backend: Mutex::new(Some(Box::new(backend))),
                state: Mutex::new(RuntimeState::default()),
                lease_directory: directory,
                shutting_down: AtomicBool::new(false),
            },
            calls,
        )
    }

    fn rejecting_runtime_at(directory: PathBuf) -> RemoteControlRuntime {
        let mut backend = FakeBackend::new();
        backend.reject_preflight = true;
        RemoteControlRuntime {
            backend: Mutex::new(Some(Box::new(backend))),
            state: Mutex::new(RuntimeState::default()),
            lease_directory: directory,
            shutting_down: AtomicBool::new(false),
        }
    }

    #[test]
    fn validates_discrete_frame_and_scale_options() {
        assert!(validate_settings(1, 100).is_ok());
        assert!(validate_settings(3, 75).is_ok());
        assert!(validate_settings(5, 50).is_ok());
        assert!(validate_settings(10, 25).is_ok());
        assert!(validate_settings(2, 50).is_err());
        assert!(validate_settings(3, 80).is_err());
    }

    #[test]
    fn jpeg_encoder_preserves_ratio_and_selected_scale() {
        let geometry = ScreenGeometry {
            pixel_width: 400,
            pixel_height: 200,
            logical_x: 0.0,
            logical_y: 0.0,
            logical_width: 400.0,
            logical_height: 200.0,
        };
        for (scale, expected) in [
            (100, (400, 200)),
            (75, (300, 150)),
            (50, (200, 100)),
            (25, (100, 50)),
        ] {
            let encoded = encode_frame(
                geometry,
                RgbaImage::from_pixel(400, 200, image::Rgba([40, 80, 120, 255])),
                scale,
            )
            .unwrap();
            assert_eq!((encoded.frame_width, encoded.frame_height), expected);
            assert!(encoded.jpeg.starts_with(&[0xff, 0xd8]));
            assert!(encoded.jpeg.ends_with(&[0xff, 0xd9]));
        }
    }

    #[test]
    fn lock_contention_errors_are_classified_as_busy() {
        assert!(remote_lock_is_busy(&std::io::Error::from(
            std::io::ErrorKind::WouldBlock
        )));
        assert!(!remote_lock_is_busy(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
        #[cfg(windows)]
        {
            assert!(remote_lock_is_busy(&std::io::Error::from_raw_os_error(32)));
            assert!(remote_lock_is_busy(&std::io::Error::from_raw_os_error(33)));
        }
    }

    #[test]
    fn only_one_runtime_can_hold_the_remote_control_lease() {
        let directory = test_directory("lease");
        let (first, _) = runtime_at(directory.clone());
        let (second, _) = runtime_at(directory.clone());
        let started = first.start(3, 50).unwrap();
        assert_eq!(
            second.start(3, 50).unwrap_err().code(),
            "remote_control_busy"
        );
        first.stop(&started.controller_token).unwrap();
        assert!(second.start(3, 50).is_ok());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn cross_process_busy_precedes_backend_preflight() {
        let directory = test_directory("busy-preflight");
        let (first, _) = runtime_at(directory.clone());
        let second = rejecting_runtime_at(directory.clone());
        let started = first.start(3, 50).unwrap();
        assert_eq!(
            second.start(3, 50).unwrap_err().code(),
            "remote_control_busy"
        );
        first.stop(&started.controller_token).unwrap();
        assert_eq!(
            second.start(3, 50).unwrap_err().code(),
            "remote_input_failed"
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn concurrent_screenshot_returns_no_frame_without_clearing_the_capture_gate() {
        let directory = test_directory("screenshot-gate");
        let (runtime, _) = runtime_at(directory.clone());
        runtime.state.lock().unwrap().capture_in_progress = true;
        assert!(runtime.screenshot(50).unwrap().is_none());
        assert!(runtime.state.lock().unwrap().capture_in_progress);
        runtime.state.lock().unwrap().capture_in_progress = false;
        assert!(runtime.screenshot(50).unwrap().is_some());
        assert!(!runtime.state.lock().unwrap().capture_in_progress);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn screenshot_does_not_acquire_control() {
        let directory = test_directory("screenshot");
        let (runtime, _) = runtime_at(directory.clone());
        let frame = runtime.screenshot(25).unwrap().unwrap();
        assert_eq!((frame.frame_width, frame.frame_height), (100, 50));
        assert!(!runtime.status(None).unwrap().active);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn controller_tracks_and_releases_pressed_inputs() {
        let directory = test_directory("release");
        let (runtime, calls) = runtime_at(directory.clone());
        let started = runtime.start(3, 50).unwrap();
        runtime
            .input(
                &started.controller_token,
                &[
                    RemoteInputEvent::MouseMove { x: 10, y: 20 },
                    RemoteInputEvent::MouseDown {
                        button: RemoteMouseButton::Left,
                    },
                    RemoteInputEvent::KeyDown {
                        code: "ShiftLeft".into(),
                    },
                ],
            )
            .unwrap();
        let released = runtime.release_inputs(&started.controller_token).unwrap();
        assert_eq!(released.released_inputs, 2);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            [
                "move:10:20",
                "down:Left",
                "key-down:shift",
                "key-up:shift",
                "up:Left"
            ]
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn invalid_input_batch_has_no_backend_side_effects() {
        let directory = test_directory("validation");
        let (runtime, calls) = runtime_at(directory.clone());
        let started = runtime.start(3, 50).unwrap();
        let error = runtime
            .input(
                &started.controller_token,
                &[
                    RemoteInputEvent::MouseDown {
                        button: RemoteMouseButton::Left,
                    },
                    RemoteInputEvent::MouseMove { x: 999, y: 20 },
                ],
            )
            .unwrap_err();
        assert_eq!(error.code(), "invalid_arguments");
        assert!(calls.lock().unwrap().is_empty());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn frame_slot_returns_only_new_sequences() {
        let directory = test_directory("frames");
        let (runtime, _) = runtime_at(directory.clone());
        let started = runtime.start(10, 50).unwrap();
        let first = runtime
            .frame(&started.controller_token, None)
            .unwrap()
            .unwrap();
        assert!(
            runtime
                .frame(&started.controller_token, Some(first.sequence))
                .unwrap()
                .is_none()
        );
        assert!(
            runtime
                .frame("wrong-token", Some(first.sequence))
                .unwrap_err()
                .code()
                == "remote_control_not_owned"
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn controller_token_owns_status_and_input_before_platform_preflight() {
        let directory = test_directory("token");
        let (runtime, calls) = runtime_at(directory.clone());
        let started = runtime.start(3, 50).unwrap();
        let anonymous = runtime.status(None).unwrap();
        assert!(anonymous.active);
        assert!(!anonymous.owned);
        assert!(!runtime.status(Some("wrong-token")).unwrap().owned);
        assert!(
            runtime
                .status(Some(&started.controller_token))
                .unwrap()
                .owned
        );
        let error = runtime
            .input(
                "wrong-token",
                &[RemoteInputEvent::KeyDown {
                    code: "KeyA".into(),
                }],
            )
            .unwrap_err();
        assert_eq!(error.code(), "remote_control_not_owned");
        assert!(calls.lock().unwrap().is_empty());
        runtime.stop(&started.controller_token).unwrap();
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn stale_controller_releases_inputs_and_cross_process_lease() {
        let directory = test_directory("timeout");
        let (runtime, calls) = runtime_at(directory.clone());
        let (next, _) = runtime_at(directory.clone());
        let started = runtime.start(3, 50).unwrap();
        runtime
            .input(
                &started.controller_token,
                &[
                    RemoteInputEvent::MouseDown {
                        button: RemoteMouseButton::Right,
                    },
                    RemoteInputEvent::KeyDown {
                        code: "ControlLeft".into(),
                    },
                ],
            )
            .unwrap();
        runtime
            .state
            .lock()
            .unwrap()
            .controller
            .as_mut()
            .unwrap()
            .last_seen = Instant::now() - CONTROLLER_TIMEOUT;
        runtime.expire_if_stale();
        assert!(!runtime.status(None).unwrap().active);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            [
                "down:Right",
                "key-down:control",
                "key-up:control",
                "up:Right"
            ]
        );
        let next_started = next.start(3, 50).unwrap();
        next.stop(&next_started.controller_token).unwrap();
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn shutdown_finalizes_inputs_and_rejects_new_control() {
        let directory = test_directory("shutdown");
        let (runtime, calls) = runtime_at(directory.clone());
        let started = runtime.start(3, 50).unwrap();
        runtime
            .input(
                &started.controller_token,
                &[RemoteInputEvent::KeyDown {
                    code: "AltLeft".into(),
                }],
            )
            .unwrap();
        runtime.shutdown();
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["key-down:option", "key-up:option"]
        );
        assert!(!runtime.status(None).unwrap().active);
        assert_eq!(
            runtime.start(3, 50).unwrap_err().code(),
            "remote_control_stopping"
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn dom_keyboard_codes_map_without_using_toolbox_names() {
        assert_eq!(normalize_dom_code("KeyA").as_deref(), Some("a"));
        assert_eq!(normalize_dom_code("Digit9").as_deref(), Some("9"));
        assert_eq!(
            normalize_dom_code("ControlRight").as_deref(),
            Some("right_control")
        );
        assert_eq!(normalize_dom_code("ArrowLeft").as_deref(), Some("left"));
        assert_eq!(normalize_dom_code("F20").as_deref(), Some("f20"));
        assert_eq!(normalize_dom_code("Fn"), None);
    }
}
