use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use image::{DynamicImage, ImageFormat, ImageReader};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    Result,
    config::{config_home, create_private_directory},
};

#[cfg(target_os = "macos")]
#[path = "desktop_toolbox/macos.rs"]
mod macos;

#[cfg(target_os = "windows")]
#[path = "desktop_toolbox/windows.rs"]
mod windows;

pub const TOOLBOX_NAME: &str = "Desktop";
pub const PLAY_TOOL: &str = "Play";

const MAX_OPERATIONS: usize = 256;
const MAX_DELAY_MS: u64 = 60_000;
const MAX_TOTAL_DELAY_MS: u64 = 300_000;
const MAX_COORDINATE: i64 = 1_000_000;
const MAX_WHEEL_DELTA: i64 = 10_000;
const MAX_TEXT_BYTES: usize = 32_768;
const DESKTOP_TEMP_DIRECTORY: &str = ".me/tmp/desktop";
#[cfg(any(target_os = "macos", test))]
const SCREEN_PERMISSION_TIP: &str = "Stop using Desktop and ask the user to enable Screen Recording for me-s or me-gateway in System Settings > Privacy & Security > Screen Recording. Retry only after the user confirms that permission was granted.";
#[cfg(any(target_os = "macos", test))]
const ACCESSIBILITY_PERMISSION_TIP: &str = "Stop using Desktop and ask the user to enable Accessibility for me-s or me-gateway in System Settings > Privacy & Security > Accessibility. Retry only after the user confirms that permission was granted.";
#[cfg(target_os = "windows")]
const INTERACTIVE_DESKTOP_TIP: &str = "Stop using Desktop. Ask the user to unlock and sign in to Windows, then start me-s or me-gateway inside the active interactive desktop and confirm it is ready. Retry only after that confirmation. Do not bypass UAC, Secure Desktop, Window Station, Session, RDP, or UIPI boundaries, and do not elevate the process.";
const CAPTURE_ID_MASK: u64 = 0x00ff_ffff;
const CAPTURE_ID_ATTEMPTS: usize = 1_024;
static CAPTURE_NONCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Deserialize)]
struct WorkerRequest {
    id: u64,
    cmd: String,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    input: Value,
}

#[derive(Clone, Debug, Deserialize)]
struct PlayInput {
    operations: Vec<Operation>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Operation {
    Capture {
        #[serde(default)]
        clip: Option<Clip>,
    },
    Delay {
        delay_ms: u64,
    },
    MouseMove {
        x: i64,
        y: i64,
    },
    MouseDown {
        #[serde(default)]
        button: MouseButton,
    },
    MouseUp {
        #[serde(default)]
        button: MouseButton,
    },
    MouseWheel {
        delta_x: i64,
        delta_y: i64,
    },
    KeyClick {
        key: String,
    },
    KeyDown {
        key: String,
    },
    KeyUp {
        key: String,
    },
    TextInput {
        text: String,
    },
}

impl Operation {
    fn requires_capture_permission(&self) -> bool {
        matches!(self, Self::Capture { .. })
    }

    fn requires_accessibility_permission(&self) -> bool {
        matches!(
            self,
            Self::MouseMove { .. }
                | Self::MouseDown { .. }
                | Self::MouseUp { .. }
                | Self::MouseWheel { .. }
                | Self::KeyClick { .. }
                | Self::KeyDown { .. }
                | Self::KeyUp { .. }
                | Self::TextInput { .. }
        )
    }

    fn requires_geometry(&self) -> bool {
        matches!(self, Self::Capture { .. } | Self::MouseMove { .. })
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

impl MouseButton {
    fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Clip {
    x: i64,
    y: i64,
    width: i64,
    height: i64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ScreenGeometry {
    pixel_width: u32,
    pixel_height: u32,
    logical_x: f64,
    logical_y: f64,
    logical_width: f64,
    logical_height: f64,
}

#[derive(Clone, Debug, Serialize)]
struct ErrorDetail {
    code: String,
    message: String,
    retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tip: Option<String>,
}

#[derive(Clone, Debug)]
struct DesktopError {
    code: &'static str,
    message: String,
    retryable: bool,
    tip: Option<&'static str>,
}

impl DesktopError {
    fn new(code: &'static str, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
            tip: None,
        }
    }

    fn with_tip(mut self, tip: &'static str) -> Self {
        self.tip = Some(tip);
        self
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_arguments", message, false)
    }

    #[cfg(any(target_os = "macos", test))]
    fn screen_permission() -> Self {
        Self::new(
            "screen_permission_required",
            "macOS Screen Recording permission is required before Desktop can capture the host screen",
            false,
        )
        .with_tip(SCREEN_PERMISSION_TIP)
    }

    #[cfg(any(target_os = "macos", test))]
    fn accessibility_permission() -> Self {
        Self::new(
            "accessibility_permission_required",
            "macOS Accessibility permission is required before Desktop can inject host input",
            false,
        )
        .with_tip(ACCESSIBILITY_PERMISSION_TIP)
    }

    #[cfg(target_os = "windows")]
    fn interactive_desktop(message: impl Into<String>) -> Self {
        Self::new("interactive_desktop_required", message, false).with_tip(INTERACTIVE_DESKTOP_TIP)
    }

    fn capture(message: impl Into<String>) -> Self {
        Self::new("desktop_capture_failed", message, false)
    }

    fn input(message: impl Into<String>) -> Self {
        Self::new("desktop_input_failed", message, false)
    }

    fn detail(&self) -> ErrorDetail {
        ErrorDetail {
            code: self.code.into(),
            message: self.message.clone(),
            retryable: self.retryable,
            tip: self.tip.map(str::to_owned),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct CaptureResult {
    operation_index: usize,
    path: String,
    width: u32,
    height: u32,
    full_width: u32,
    full_height: u32,
    clip: CaptureRegion,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct CaptureRegion {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[derive(Clone, Debug, Serialize)]
struct PlayResult {
    state: &'static str,
    operation_count: usize,
    completed_operations: usize,
    failed_operation_index: Option<usize>,
    captures: Vec<CaptureResult>,
    auto_released: Vec<String>,
    cleanup_errors: Vec<ErrorDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorDetail>,
}

trait DesktopBackend {
    fn geometry(&self) -> std::result::Result<ScreenGeometry, DesktopError>;
    fn preflight_capture(&self) -> std::result::Result<(), DesktopError>;
    fn preflight_input(&self) -> std::result::Result<(), DesktopError>;
    fn capture_full(&mut self, destination: &Path) -> std::result::Result<(), DesktopError>;
    fn delay(&mut self, duration: Duration) -> std::result::Result<(), DesktopError>;
    fn mouse_move(
        &mut self,
        x: u32,
        y: u32,
        held: &[MouseButton],
    ) -> std::result::Result<(), DesktopError>;
    fn mouse_down(&mut self, button: MouseButton) -> std::result::Result<(), DesktopError>;
    fn mouse_up(&mut self, button: MouseButton) -> std::result::Result<(), DesktopError>;
    fn mouse_wheel(&mut self, delta_x: i32, delta_y: i32) -> std::result::Result<(), DesktopError>;
    fn key_down(&mut self, key: &str) -> std::result::Result<(), DesktopError>;
    fn key_up(&mut self, key: &str) -> std::result::Result<(), DesktopError>;
    fn text_input(&mut self, text: &str) -> std::result::Result<(), DesktopError>;
}

pub fn run(input: impl Read, mut output: impl Write, workspace: &Path) -> Result<()> {
    for line in BufReader::new(input).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: WorkerRequest = serde_json::from_str(&line)
            .map_err(|error| format!("Desktop toolbox received invalid JSONL: {error}"))?;
        let frame = if request.cmd == "execute" {
            execute_response(&request, workspace)
        } else {
            metadata_response(&request)
        };
        serde_json::to_writer(&mut output, &frame)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    Ok(())
}

fn metadata_response(request: &WorkerRequest) -> Value {
    let result = match request.cmd.as_str() {
        "getTools" => Ok(if cfg!(any(target_os = "macos", target_os = "windows")) {
            json!([PLAY_TOOL])
        } else {
            json!([])
        }),
        "getBrief" => Ok(Value::String(brief().into())),
        "getInputSchema" => metadata_tool(request).and_then(input_schema),
        "getOutputSchema" => metadata_tool(request).and_then(output_schema),
        "getInstructions" => metadata_tool(request)
            .and_then(instructions)
            .map(|value| Value::String(value.into())),
        "getRoute" => metadata_tool(request)
            .and_then(route)
            .map(|value| Value::String(value.into())),
        "getExamples" => metadata_tool(request)
            .and_then(examples)
            .map(|value| Value::String(value.into())),
        _ => Err(DesktopError::new(
            "invalid_request",
            format!("unknown toolbox command {}", request.cmd),
            false,
        )),
    };
    match result {
        Ok(value) => json!({"id": request.id, "type": "result", "output": value}),
        Err(error) => error_frame(request.id, &error),
    }
}

fn metadata_tool(request: &WorkerRequest) -> std::result::Result<&str, DesktopError> {
    request.tool.as_deref().ok_or_else(|| {
        DesktopError::new(
            "invalid_request",
            format!("{} requires tool", request.cmd),
            false,
        )
    })
}

fn execute_response(request: &WorkerRequest, workspace: &Path) -> Value {
    if request.tool.as_deref() != Some(PLAY_TOOL) {
        return error_frame(
            request.id,
            &DesktopError::new(
                "invalid_request",
                format!("Desktop execute requires tool {PLAY_TOOL}"),
                false,
            ),
        );
    }
    match execute_system(request.input.clone(), workspace) {
        Ok(result) => json!({"id": request.id, "type": "result", "output": result}),
        Err(error) => error_frame(request.id, &error),
    }
}

fn error_frame(id: u64, error: &DesktopError) -> Value {
    json!({
        "id": id,
        "type": "error",
        "error": {
            "code": error.code,
            "message": error.message,
            "retryable": error.retryable,
            "tip": error.tip,
        }
    })
}

fn execute_system(input: Value, workspace: &Path) -> std::result::Result<Value, DesktopError> {
    let input = parse_and_validate(input)?;
    let _lease = DesktopLease::acquire()?;
    let mut backend = system_backend()?;
    let result = execute_validated(&input, workspace, backend.as_mut())?;
    serde_json::to_value(result)
        .map_err(|error| DesktopError::new("desktop_input_failed", error.to_string(), false))
}

fn execute_validated(
    input: &PlayInput,
    workspace: &Path,
    backend: &mut dyn DesktopBackend,
) -> std::result::Result<PlayResult, DesktopError> {
    let geometry = if input.operations.iter().any(Operation::requires_geometry) {
        let geometry = backend.geometry()?;
        validate_geometry(input, geometry)?;
        Some(geometry)
    } else {
        None
    };
    if input
        .operations
        .iter()
        .any(Operation::requires_capture_permission)
    {
        backend.preflight_capture()?;
    }
    if input
        .operations
        .iter()
        .any(Operation::requires_accessibility_permission)
    {
        backend.preflight_input()?;
    }
    execute_play(input, workspace, backend, geometry)
}

#[cfg(target_os = "macos")]
fn system_backend() -> std::result::Result<Box<dyn DesktopBackend>, DesktopError> {
    Ok(Box::new(macos::MacDesktopBackend::new()?))
}

#[cfg(target_os = "windows")]
fn system_backend() -> std::result::Result<Box<dyn DesktopBackend>, DesktopError> {
    Ok(Box::new(windows::WindowsDesktopBackend::new()?))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn system_backend() -> std::result::Result<Box<dyn DesktopBackend>, DesktopError> {
    Err(DesktopError::new(
        "desktop_unavailable",
        "Desktop.Play is not supported on this host platform",
        false,
    ))
}

fn parse_and_validate(input: Value) -> std::result::Result<PlayInput, DesktopError> {
    validate_input_shape(&input)?;
    let mut input: PlayInput =
        serde_json::from_value(input).map_err(|error| DesktopError::invalid(error.to_string()))?;
    if input.operations.is_empty() {
        return Err(DesktopError::invalid("operations must not be empty"));
    }
    if input.operations.len() > MAX_OPERATIONS {
        return Err(DesktopError::invalid(format!(
            "operations contains {} items; the maximum is {MAX_OPERATIONS}",
            input.operations.len()
        )));
    }
    let capture_indices = input
        .operations
        .iter()
        .enumerate()
        .filter_map(|(index, operation)| {
            matches!(operation, Operation::Capture { .. }).then_some(index)
        })
        .collect::<Vec<_>>();
    if capture_indices.len() > 1 {
        return Err(DesktopError::new(
            "multiple_captures",
            "operations may contain at most one capture",
            false,
        ));
    }
    if capture_indices
        .first()
        .is_some_and(|index| *index + 1 != input.operations.len())
    {
        return Err(DesktopError::new(
            "capture_must_be_final",
            "capture must be the final operation",
            false,
        ));
    }

    let mut total_delay = 0_u64;
    for (index, operation) in input.operations.iter_mut().enumerate() {
        match operation {
            Operation::Capture { clip } => {
                if let Some(clip) = clip {
                    validate_clip_static(*clip).map_err(|error| indexed_error(index, error))?;
                }
            }
            Operation::Delay { delay_ms } => {
                if *delay_ms > MAX_DELAY_MS {
                    return Err(DesktopError::invalid(format!(
                        "operations[{index}].delay_ms exceeds {MAX_DELAY_MS}"
                    )));
                }
                total_delay = total_delay.checked_add(*delay_ms).ok_or_else(|| {
                    DesktopError::invalid("total delay duration overflows its supported range")
                })?;
                if total_delay > MAX_TOTAL_DELAY_MS {
                    return Err(DesktopError::invalid(format!(
                        "total delay exceeds {MAX_TOTAL_DELAY_MS} ms"
                    )));
                }
            }
            Operation::MouseMove { x, y } => {
                validate_coordinate(index, "x", *x)?;
                validate_coordinate(index, "y", *y)?;
            }
            Operation::MouseWheel { delta_x, delta_y } => {
                if *delta_x == 0 && *delta_y == 0 {
                    return Err(DesktopError::invalid(format!(
                        "operations[{index}] mouse_wheel requires a non-zero delta"
                    )));
                }
                for (name, value) in [("delta_x", *delta_x), ("delta_y", *delta_y)] {
                    if value.unsigned_abs() > MAX_WHEEL_DELTA as u64 {
                        return Err(DesktopError::invalid(format!(
                            "operations[{index}].{name} must be between -{MAX_WHEEL_DELTA} and {MAX_WHEEL_DELTA}"
                        )));
                    }
                }
            }
            Operation::KeyClick { key } | Operation::KeyDown { key } | Operation::KeyUp { key } => {
                let normalized = normalize_key(key).ok_or_else(|| {
                    DesktopError::invalid(format!(
                        "operations[{index}].key is not a supported physical key"
                    ))
                })?;
                if !platform_supports_key(&normalized) {
                    return Err(DesktopError::invalid(format!(
                        "operations[{index}].key is not supported on this host platform"
                    )));
                }
                *key = normalized;
            }
            Operation::TextInput { text } => {
                if text.is_empty() {
                    return Err(DesktopError::invalid(format!(
                        "operations[{index}].text must not be empty"
                    )));
                }
                if text.len() > MAX_TEXT_BYTES {
                    return Err(DesktopError::invalid(format!(
                        "operations[{index}].text exceeds {MAX_TEXT_BYTES} UTF-8 bytes"
                    )));
                }
            }
            Operation::MouseDown { .. } | Operation::MouseUp { .. } => {}
        }
    }
    Ok(input)
}

fn validate_input_shape(input: &Value) -> std::result::Result<(), DesktopError> {
    let object = input
        .as_object()
        .ok_or_else(|| DesktopError::invalid("Desktop.Play input must be an object"))?;
    for key in object.keys() {
        if key != "operations" {
            return Err(DesktopError::invalid(format!(
                "unknown top-level field {key:?}"
            )));
        }
    }
    let operations = object
        .get("operations")
        .and_then(Value::as_array)
        .ok_or_else(|| DesktopError::invalid("operations must be an array"))?;
    for (index, operation) in operations.iter().enumerate() {
        let object = operation.as_object().ok_or_else(|| {
            DesktopError::invalid(format!("operations[{index}] must be an object"))
        })?;
        let kind = object.get("kind").and_then(Value::as_str).ok_or_else(|| {
            DesktopError::invalid(format!("operations[{index}].kind must be a string"))
        })?;
        let allowed: &[&str] = match kind {
            "capture" => &["kind", "clip"],
            "delay" => &["kind", "delay_ms"],
            "mouse_move" => &["kind", "x", "y"],
            "mouse_down" | "mouse_up" => &["kind", "button"],
            "mouse_wheel" => &["kind", "delta_x", "delta_y"],
            "key_click" | "key_down" | "key_up" => &["kind", "key"],
            "text_input" => &["kind", "text"],
            _ => {
                return Err(DesktopError::new(
                    "unsupported_operation",
                    format!("operations[{index}] has unsupported kind {kind:?}"),
                    false,
                ));
            }
        };
        for key in object.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(DesktopError::invalid(format!(
                    "operations[{index}] kind {kind:?} does not allow field {key:?}"
                )));
            }
        }
        if kind == "capture"
            && let Some(clip) = object.get("clip")
        {
            let clip = clip.as_object().ok_or_else(|| {
                DesktopError::invalid(format!("operations[{index}].clip must be an object"))
            })?;
            for key in clip.keys() {
                if !["x", "y", "width", "height"].contains(&key.as_str()) {
                    return Err(DesktopError::invalid(format!(
                        "operations[{index}].clip has unknown field {key:?}"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn indexed_error(index: usize, error: DesktopError) -> DesktopError {
    DesktopError {
        message: format!("operations[{index}]: {}", error.message),
        ..error
    }
}

fn validate_coordinate(
    index: usize,
    name: &str,
    value: i64,
) -> std::result::Result<(), DesktopError> {
    if !(0..=MAX_COORDINATE).contains(&value) {
        return Err(DesktopError::invalid(format!(
            "operations[{index}].{name} must be between 0 and {MAX_COORDINATE}"
        )));
    }
    Ok(())
}

fn validate_clip_static(clip: Clip) -> std::result::Result<(), DesktopError> {
    if clip.x < 0 || clip.y < 0 {
        return Err(DesktopError::invalid("clip x and y must be non-negative"));
    }
    if clip.width <= 0 || clip.height <= 0 {
        return Err(DesktopError::invalid(
            "clip width and height must be greater than zero",
        ));
    }
    for (name, value) in [
        ("x", clip.x),
        ("y", clip.y),
        ("width", clip.width),
        ("height", clip.height),
    ] {
        if value > MAX_COORDINATE {
            return Err(DesktopError::invalid(format!(
                "clip {name} exceeds {MAX_COORDINATE}"
            )));
        }
    }
    clip.x
        .checked_add(clip.width)
        .and_then(|_| clip.y.checked_add(clip.height))
        .ok_or_else(|| DesktopError::invalid("clip bounds overflow"))?;
    Ok(())
}

fn validate_geometry(
    input: &PlayInput,
    geometry: ScreenGeometry,
) -> std::result::Result<(), DesktopError> {
    if geometry.pixel_width == 0
        || geometry.pixel_height == 0
        || !geometry.logical_width.is_finite()
        || !geometry.logical_height.is_finite()
        || geometry.logical_width <= 0.0
        || geometry.logical_height <= 0.0
    {
        return Err(DesktopError::new(
            "desktop_capture_failed",
            "the primary display reported invalid geometry",
            false,
        ));
    }
    for (index, operation) in input.operations.iter().enumerate() {
        match operation {
            Operation::MouseMove { x, y } => {
                if *x >= i64::from(geometry.pixel_width) || *y >= i64::from(geometry.pixel_height) {
                    return Err(DesktopError::invalid(format!(
                        "operations[{index}] mouse coordinate ({x}, {y}) is outside the {}x{} primary display",
                        geometry.pixel_width, geometry.pixel_height
                    )));
                }
            }
            Operation::Capture { clip: Some(clip) } => {
                let right = clip.x.checked_add(clip.width).ok_or_else(|| {
                    DesktopError::invalid(format!("operations[{index}] clip bounds overflow"))
                })?;
                let bottom = clip.y.checked_add(clip.height).ok_or_else(|| {
                    DesktopError::invalid(format!("operations[{index}] clip bounds overflow"))
                })?;
                if right > i64::from(geometry.pixel_width)
                    || bottom > i64::from(geometry.pixel_height)
                {
                    return Err(DesktopError::invalid(format!(
                        "operations[{index}] clip is outside the {}x{} full screenshot",
                        geometry.pixel_width, geometry.pixel_height
                    )));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn execute_play(
    input: &PlayInput,
    workspace: &Path,
    backend: &mut dyn DesktopBackend,
    geometry: Option<ScreenGeometry>,
) -> std::result::Result<PlayResult, DesktopError> {
    let mut pressed = PressedInput::default();
    let mut captures = Vec::new();
    let mut completed = 0;
    let mut failure = None;

    for (index, operation) in input.operations.iter().enumerate() {
        let result = execute_operation(
            operation,
            index,
            workspace,
            backend,
            geometry,
            &mut pressed,
            &mut captures,
        );
        match result {
            Ok(()) => completed += 1,
            Err(error) => {
                failure = Some((index, error));
                break;
            }
        }
    }

    let (auto_released, cleanup_errors) = pressed.release_all(backend);
    if failure.is_none() && !cleanup_errors.is_empty() {
        failure = Some((
            input.operations.len(),
            DesktopError::input("failed to release all synthetic input after Desktop.Play"),
        ));
    }
    let (state, failed_operation_index, error) = if let Some((index, error)) = failure {
        (
            "failed",
            (index < input.operations.len()).then_some(index),
            Some(error.detail()),
        )
    } else {
        ("succeeded", None, None)
    };
    Ok(PlayResult {
        state,
        operation_count: input.operations.len(),
        completed_operations: completed,
        failed_operation_index,
        captures,
        auto_released,
        cleanup_errors,
        error,
    })
}

#[allow(clippy::too_many_arguments)]
fn execute_operation(
    operation: &Operation,
    index: usize,
    workspace: &Path,
    backend: &mut dyn DesktopBackend,
    geometry: Option<ScreenGeometry>,
    pressed: &mut PressedInput,
    captures: &mut Vec<CaptureResult>,
) -> std::result::Result<(), DesktopError> {
    match operation {
        Operation::Capture { clip } => {
            let geometry = geometry.ok_or_else(|| {
                DesktopError::capture("screen geometry was not established before capture")
            })?;
            captures.push(capture_image(backend, workspace, index, geometry, *clip)?);
        }
        Operation::Delay { delay_ms } => {
            backend.delay(Duration::from_millis(*delay_ms))?;
        }
        Operation::MouseMove { x, y } => {
            backend.mouse_move(*x as u32, *y as u32, &pressed.buttons)?;
        }
        Operation::MouseDown { button } => {
            backend.mouse_down(*button)?;
            pressed.press_button(*button);
        }
        Operation::MouseUp { button } => {
            backend.mouse_up(*button)?;
            pressed.release_button(*button);
        }
        Operation::MouseWheel { delta_x, delta_y } => {
            backend.mouse_wheel(*delta_x as i32, *delta_y as i32)?;
        }
        Operation::KeyClick { key } => {
            backend.key_down(key)?;
            pressed.press_key(key);
            backend.key_up(key)?;
            pressed.release_key(key);
        }
        Operation::KeyDown { key } => {
            backend.key_down(key)?;
            pressed.press_key(key);
        }
        Operation::KeyUp { key } => {
            backend.key_up(key)?;
            pressed.release_key(key);
        }
        Operation::TextInput { text } => {
            backend.text_input(text)?;
        }
    }
    Ok(())
}

#[derive(Default)]
struct PressedInput {
    keys: Vec<String>,
    buttons: Vec<MouseButton>,
}

impl PressedInput {
    fn press_key(&mut self, key: &str) {
        if !self.keys.iter().any(|value| value == key) {
            self.keys.push(key.to_owned());
        }
    }

    fn release_key(&mut self, key: &str) {
        self.keys.retain(|value| value != key);
    }

    fn press_button(&mut self, button: MouseButton) {
        if !self.buttons.contains(&button) {
            self.buttons.push(button);
        }
    }

    fn release_button(&mut self, button: MouseButton) {
        self.buttons.retain(|value| *value != button);
    }

    fn release_all(&mut self, backend: &mut dyn DesktopBackend) -> (Vec<String>, Vec<ErrorDetail>) {
        let mut released = Vec::new();
        let mut errors = Vec::new();
        for key in self.keys.iter().rev() {
            match backend.key_up(key) {
                Ok(()) => released.push(format!("key:{key}")),
                Err(error) => errors.push(error.detail()),
            }
        }
        for button in self.buttons.iter().rev() {
            match backend.mouse_up(*button) {
                Ok(()) => released.push(format!("mouse:{}", button.name())),
                Err(error) => errors.push(error.detail()),
            }
        }
        self.keys.clear();
        self.buttons.clear();
        (released, errors)
    }
}

fn capture_image(
    backend: &mut dyn DesktopBackend,
    workspace: &Path,
    operation_index: usize,
    geometry: ScreenGeometry,
    requested_clip: Option<Clip>,
) -> std::result::Result<CaptureResult, DesktopError> {
    let directory = workspace.join(DESKTOP_TEMP_DIRECTORY);
    create_private_directory(&directory).map_err(|error| {
        DesktopError::capture(format!("cannot create Desktop capture directory: {error}"))
    })?;
    let (full_path, final_name, final_path) = (0..CAPTURE_ID_ATTEMPTS)
        .find_map(|_| {
            let id = capture_id();
            let full_path = directory.join(format!("full-{id}.png"));
            let final_name = format!("capture-{id}.png");
            let final_path = directory.join(&final_name);
            (!full_path.exists() && !final_path.exists())
                .then_some((full_path, final_name, final_path))
        })
        .ok_or_else(|| DesktopError::capture("cannot allocate an unused six-digit capture id"))?;
    let mut full_guard = TemporaryFile::new(full_path.clone());
    backend.capture_full(&full_path)?;
    set_private_file_permissions(&full_path).map_err(|error| {
        capture_validation_error(
            backend,
            format!("cannot make the full screenshot private: {error}"),
        )
    })?;
    let metadata = fs::metadata(&full_path).map_err(|error| {
        capture_validation_error(backend, format!("full screenshot was not created: {error}"))
    })?;
    if metadata.len() == 0 {
        return Err(capture_validation_error(
            backend,
            "full screenshot is empty",
        ));
    }
    let file = File::open(&full_path).map_err(|error| {
        capture_validation_error(backend, format!("cannot open full screenshot: {error}"))
    })?;
    let image = ImageReader::with_format(BufReader::new(file), ImageFormat::Png)
        .decode()
        .map_err(|error| {
            capture_validation_error(
                backend,
                format!("full screenshot is not a valid PNG: {error}"),
            )
        })?;
    if image.width() != geometry.pixel_width || image.height() != geometry.pixel_height {
        return Err(capture_validation_error(
            backend,
            format!(
                "full screenshot dimensions {}x{} do not match the primary display {}x{}",
                image.width(),
                image.height(),
                geometry.pixel_width,
                geometry.pixel_height
            ),
        ));
    }
    if is_fully_transparent(&image) {
        return Err(capture_validation_error(
            backend,
            "full screenshot is fully transparent",
        ));
    }

    let region = requested_clip.map_or(
        CaptureRegion {
            x: 0,
            y: 0,
            width: geometry.pixel_width,
            height: geometry.pixel_height,
        },
        |clip| CaptureRegion {
            x: clip.x as u32,
            y: clip.y as u32,
            width: clip.width as u32,
            height: clip.height as u32,
        },
    );
    if requested_clip.is_some() {
        let cropped = image.crop_imm(region.x, region.y, region.width, region.height);
        write_private_png(&final_path, &cropped)?;
    } else {
        fs::rename(&full_path, &final_path).map_err(|error| {
            DesktopError::capture(format!("cannot finalize full screenshot: {error}"))
        })?;
        full_guard.disarm();
        set_private_file_permissions(&final_path).map_err(|error| {
            let _ = fs::remove_file(&final_path);
            DesktopError::capture(format!("cannot make final screenshot private: {error}"))
        })?;
    }
    Ok(CaptureResult {
        operation_index,
        path: format!("{DESKTOP_TEMP_DIRECTORY}/{final_name}"),
        width: region.width,
        height: region.height,
        full_width: geometry.pixel_width,
        full_height: geometry.pixel_height,
        clip: region,
    })
}

fn capture_validation_error(
    backend: &dyn DesktopBackend,
    message: impl Into<String>,
) -> DesktopError {
    match backend.preflight_capture() {
        Err(error)
            if matches!(
                error.code,
                "screen_permission_required" | "interactive_desktop_required"
            ) =>
        {
            error
        }
        _ => DesktopError::capture(message),
    }
}

fn is_fully_transparent(image: &DynamicImage) -> bool {
    image.to_rgba8().pixels().all(|pixel| pixel.0[3] == 0)
}

fn write_private_png(path: &Path, image: &DynamicImage) -> std::result::Result<(), DesktopError> {
    if path.exists() {
        return Err(DesktopError::capture(format!(
            "capture destination {} already exists",
            path.display()
        )));
    }
    let temporary = path.with_extension(format!("part-{}", std::process::id()));
    let _guard = TemporaryFile::new(temporary.clone());
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(|error| {
        DesktopError::capture(format!("cannot create cropped screenshot: {error}"))
    })?;
    image
        .write_to(&mut file, ImageFormat::Png)
        .map_err(|error| DesktopError::capture(format!("cannot encode cropped PNG: {error}")))?;
    file.sync_all().map_err(|error| {
        DesktopError::capture(format!("cannot flush cropped screenshot: {error}"))
    })?;
    drop(file);
    fs::rename(&temporary, path).map_err(|error| {
        DesktopError::capture(format!("cannot finalize cropped screenshot: {error}"))
    })?;
    set_private_file_permissions(path).map_err(|error| {
        let _ = fs::remove_file(path);
        DesktopError::capture(format!("cannot make cropped screenshot private: {error}"))
    })?;
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

struct TemporaryFile {
    path: Option<PathBuf>,
}

impl TemporaryFile {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

fn capture_id() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let nonce = CAPTURE_NONCE.fetch_add(1, Ordering::Relaxed);
    let mixed = timestamp
        .wrapping_add(u64::from(std::process::id()).rotate_left(17))
        .wrapping_add(nonce.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    format!("{:06x}", mixed & CAPTURE_ID_MASK)
}

#[derive(Debug)]
struct DesktopLease {
    file: File,
}

impl DesktopLease {
    fn acquire() -> std::result::Result<Self, DesktopError> {
        let home = config_home().map_err(|error| {
            DesktopError::new(
                "desktop_lock_failed",
                format!("cannot resolve the global Desktop lock directory: {error}"),
                false,
            )
        })?;
        Self::acquire_at(&home.join("desktop"))
    }

    fn acquire_at(directory: &Path) -> std::result::Result<Self, DesktopError> {
        create_private_directory(directory).map_err(|error| {
            DesktopError::new(
                "desktop_lock_failed",
                format!("cannot create the global Desktop lock directory: {error}"),
                false,
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
                DesktopError::new(
                    "desktop_lock_failed",
                    format!("cannot open the global Desktop lock: {error}"),
                    false,
                )
            })?;
        match FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Self { file }),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Err(DesktopError::new(
                "desktop_busy",
                "another Desktop.Play currently controls this host desktop",
                true,
            )),
            Err(error) => Err(DesktopError::new(
                "desktop_lock_failed",
                format!("cannot acquire the global Desktop lock: {error}"),
                false,
            )),
        }
    }
}

impl Drop for DesktopLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn normalize_key(value: &str) -> Option<String> {
    let normalized = value.trim().to_ascii_lowercase().replace('-', "_");
    if normalized.len() == 1 {
        let byte = normalized.as_bytes()[0];
        if byte.is_ascii_alphanumeric() {
            return Some(normalized);
        }
        return match byte {
            b'-' => Some("minus".into()),
            b'=' => Some("equal".into()),
            b'[' => Some("left_bracket".into()),
            b']' => Some("right_bracket".into()),
            b'\\' => Some("backslash".into()),
            b';' => Some("semicolon".into()),
            b'\'' => Some("quote".into()),
            b',' => Some("comma".into()),
            b'.' => Some("period".into()),
            b'/' => Some("slash".into()),
            b'`' => Some("grave".into()),
            _ => None,
        };
    }
    let canonical = match normalized.as_str() {
        "enter" | "return" => "return",
        "esc" | "escape" => "escape",
        "space" => "space",
        "tab" => "tab",
        "backspace" => "backspace",
        "delete" | "forward_delete" => "delete",
        "left" | "arrow_left" => "left",
        "right" | "arrow_right" => "right",
        "up" | "arrow_up" => "up",
        "down" | "arrow_down" => "down",
        "home" => "home",
        "end" => "end",
        "page_up" | "pageup" => "page_up",
        "page_down" | "pagedown" => "page_down",
        "shift" | "left_shift" => "shift",
        "right_shift" => "right_shift",
        "control" | "ctrl" | "left_control" | "left_ctrl" => "control",
        "right_control" | "right_ctrl" => "right_control",
        "option" | "alt" | "left_option" | "left_alt" => "option",
        "right_option" | "right_alt" => "right_option",
        "command" | "cmd" | "meta" | "left_command" | "left_meta" => "command",
        "right_command" | "right_cmd" | "right_meta" => "right_command",
        "caps_lock" | "capslock" => "caps_lock",
        "fn" | "function" => "function",
        "minus" => "minus",
        "equal" | "equals" => "equal",
        "left_bracket" => "left_bracket",
        "right_bracket" => "right_bracket",
        "backslash" => "backslash",
        "semicolon" => "semicolon",
        "quote" | "apostrophe" => "quote",
        "comma" => "comma",
        "period" | "dot" => "period",
        "slash" => "slash",
        "grave" | "backtick" => "grave",
        value if function_key(value) => value,
        _ => return None,
    };
    Some(canonical.into())
}

fn function_key(value: &str) -> bool {
    value
        .strip_prefix('f')
        .and_then(|number| number.parse::<u8>().ok())
        .is_some_and(|number| (1..=20).contains(&number))
}

fn platform_supports_key(key: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        key != "function"
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = key;
        true
    }
}

fn brief() -> &'static str {
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        "Operate the real host desktop through one validated, ordered Play batch. Desktop is not the browser viewport or the device displaying the WebUI."
    } else {
        "Host-desktop automation is unavailable on this platform, so Desktop exposes no model tools."
    }
}

fn input_schema(tool: &str) -> std::result::Result<Value, DesktopError> {
    if tool != PLAY_TOOL {
        return Err(DesktopError::new(
            "invalid_request",
            format!("unknown Desktop tool {tool}"),
            false,
        ));
    }
    let clip = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "x": {"type": "integer", "minimum": 0, "maximum": MAX_COORDINATE},
            "y": {"type": "integer", "minimum": 0, "maximum": MAX_COORDINATE},
            "width": {"type": "integer", "minimum": 1, "maximum": MAX_COORDINATE},
            "height": {"type": "integer", "minimum": 1, "maximum": MAX_COORDINATE}
        },
        "required": ["x", "y", "width", "height"]
    });
    let button = json!({"type": "string", "enum": ["left", "right", "middle"]});
    Ok(json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "operations": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_OPERATIONS,
                "items": {
                    "oneOf": [
                        {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"capture"},"clip":clip},"required":["kind"]},
                        {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"delay"},"delay_ms":{"type":"integer","minimum":0,"maximum":MAX_DELAY_MS}},"required":["kind","delay_ms"]},
                        {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"mouse_move"},"x":{"type":"integer","minimum":0,"maximum":MAX_COORDINATE},"y":{"type":"integer","minimum":0,"maximum":MAX_COORDINATE}},"required":["kind","x","y"]},
                        {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"mouse_down"},"button":button},"required":["kind"]},
                        {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"mouse_up"},"button":button},"required":["kind"]},
                        {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"mouse_wheel"},"delta_x":{"type":"integer","minimum":-MAX_WHEEL_DELTA,"maximum":MAX_WHEEL_DELTA},"delta_y":{"type":"integer","minimum":-MAX_WHEEL_DELTA,"maximum":MAX_WHEEL_DELTA}},"required":["kind","delta_x","delta_y"]},
                        {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"key_click"},"key":{"type":"string","minLength":1,"maxLength":32}},"required":["kind","key"]},
                        {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"key_down"},"key":{"type":"string","minLength":1,"maxLength":32}},"required":["kind","key"]},
                        {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"key_up"},"key":{"type":"string","minLength":1,"maxLength":32}},"required":["kind","key"]},
                        {"type":"object","additionalProperties":false,"properties":{"kind":{"const":"text_input"},"text":{"type":"string","minLength":1,"maxLength":MAX_TEXT_BYTES}},"required":["kind","text"]}
                    ]
                }
            }
        },
        "required": ["operations"]
    }))
}

fn output_schema(tool: &str) -> std::result::Result<Value, DesktopError> {
    if tool != PLAY_TOOL {
        return Err(DesktopError::new(
            "invalid_request",
            format!("unknown Desktop tool {tool}"),
            false,
        ));
    }
    Ok(json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "state": {"type":"string","enum":["succeeded","failed"]},
            "operation_count": {"type":"integer","minimum":1},
            "completed_operations": {"type":"integer","minimum":0},
            "failed_operation_index": {"type":["integer","null"],"minimum":0},
            "captures": {"type":"array","maxItems":1,"items":{"type":"object","additionalProperties":false,"properties":{
                "operation_index":{"type":"integer","minimum":0},
                "path":{"type":"string"},
                "width":{"type":"integer","minimum":1},
                "height":{"type":"integer","minimum":1},
                "full_width":{"type":"integer","minimum":1},
                "full_height":{"type":"integer","minimum":1},
                "clip":{"type":"object","additionalProperties":false,"properties":{"x":{"type":"integer","minimum":0},"y":{"type":"integer","minimum":0},"width":{"type":"integer","minimum":1},"height":{"type":"integer","minimum":1}},"required":["x","y","width","height"]}
            },"required":["operation_index","path","width","height","full_width","full_height","clip"]}},
            "auto_released": {"type":"array","items":{"type":"string"}},
            "cleanup_errors": {"type":"array","items":{"type":"object"}},
            "error": {"type":"object","properties":{"code":{"type":"string"},"message":{"type":"string"},"retryable":{"type":"boolean"},"tip":{"type":"string"}},"required":["code","message","retryable"]}
        },
        "required": ["state","operation_count","completed_operations","failed_operation_index","captures","auto_released","cleanup_errors"]
    }))
}

fn instructions(tool: &str) -> std::result::Result<&'static str, DesktopError> {
    if tool != PLAY_TOOL {
        return Err(DesktopError::new(
            "invalid_request",
            format!("unknown Desktop tool {tool}"),
            false,
        ));
    }
    Ok(
        r#"Desktop.Play controls the real desktop of the host running me-s or me-gateway, not a Camoufox page, browser viewport, or the client device displaying the WebUI.

The complete operations array is parsed and validated before any desktop input is injected. Operations then execute exactly in array order. Desktop effects cannot be rolled back: if one operation fails, prior effects remain and later operations do not run. Always inspect `state`, `completed_operations`, `failed_operation_index`, `error`, `auto_released`, and `cleanup_errors`.

`capture` is optional. A Play may contain no capture. If present, capture may appear only once and must be the final operation. Capture first takes one full primary-display screenshot, then applies an optional in-memory `clip`; it never uses an operating-system region-capture API and never scales the crop. Clip coordinates and all mouse coordinates use full-screenshot pixels with a top-left origin. Invalid, zero-sized, or out-of-bounds clips reject the whole batch before input.

A successful capture returns a workspace-relative PNG path under `.me/tmp/desktop`, the output dimensions, full screenshot dimensions, and the exact output region in full-screen coordinates. Call Image.View with that path to inspect the pixels. The Desktop result itself does not add image content to model context. If the viewed image is blank, malformed, or otherwise unexpected, stop rather than guessing desktop state.

Screen Recording and Accessibility are separate macOS permissions. On `screen_permission_required` or `accessibility_permission_required`, stop Desktop work and ask the user to grant the named permission manually. Do not retry blindly, bypass the permission, substitute a privileged path, or claim success. Only retry after the user confirms permission was granted.

On Windows, Desktop.Play must run inside the active, unlocked user's `WinSta0\\Default` interactive desktop. `interactive_desktop_required` means the process is in an SSH/service/non-interactive Session, Windows is locked or signed out, RDP output is disconnected, the input Desktop is Winlogon or UAC Secure Desktop, or no usable display is available. Stop immediately, ask the user to unlock/sign in and start me-s or me-gateway in the active desktop, and retry only after confirmation. Never elevate, cross Sessions, switch desktops, bypass UAC/UIPI, or continue from a black or abnormal capture. `desktop_input_failed` may mean UIPI, security software, policy, a desktop switch, or a higher-integrity target rejected `SendInput`; do not conceal or bypass that boundary.

Use deterministic batches for typing, shortcuts, clicks, double-clicks, drags, long presses, scrolling, and delays. If an action may unpredictably change focus, windows, menus, or control positions, end that Play with capture and inspect it before constructing another batch. Capture is still not mandatory for deterministic actions.

Desktop is a host-global resource. Play obtains an internal cross-process exclusive lease; `desktop_busy` means another Agent or me process currently controls the desktop. Tool availability does not authorize destructive or externally visible actions. Obtain the same user authority required without Desktop before deleting data, sending messages, submitting forms, changing permissions, or causing another consequential effect.

The executor tracks synthetic key and mouse-down state and attempts to release leftovers on normal completion and runtime failures. `auto_released` reports successful finalizer releases. An operating-system force kill cannot be made perfectly recoverable, so do not leave inputs down longer than needed."#,
    )
}

fn route(tool: &str) -> std::result::Result<&'static str, DesktopError> {
    if tool != PLAY_TOOL {
        return Err(DesktopError::new(
            "invalid_request",
            format!("unknown Desktop tool {tool}"),
            false,
        ));
    }
    Ok(
        "Play one fully validated ordered batch against the real host desktop, with an optional final full-screen capture or strict in-memory clip.",
    )
}

fn examples(tool: &str) -> std::result::Result<&'static str, DesktopError> {
    if tool != PLAY_TOOL {
        return Err(DesktopError::new(
            "invalid_request",
            format!("unknown Desktop tool {tool}"),
            false,
        ));
    }
    Ok(r#"Capture only:
{"operations":[{"kind":"capture"}]}

Type deterministic text without capturing:
{"operations":[{"kind":"text_input","text":"Hello，世界 👋"},{"kind":"key_click","key":"enter"}]}

Drag, wait, then capture a strict full-screen pixel region:
{"operations":[{"kind":"mouse_move","x":800,"y":500},{"kind":"mouse_down"},{"kind":"mouse_move","x":1600,"y":500},{"kind":"mouse_up"},{"kind":"delay","delay_ms":500},{"kind":"capture","clip":{"x":800,"y":300,"width":1000,"height":700}}]}"#)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        sync::{Arc, Mutex},
    };

    use image::{ImageBuffer, Rgba};

    use super::*;

    #[derive(Clone, Copy)]
    enum CaptureFixture {
        Valid,
        Empty,
        Transparent,
        WrongSize,
    }

    #[derive(Clone)]
    struct FakeBackend {
        calls: Arc<Mutex<Vec<String>>>,
        fail_on: Option<String>,
        capture_permission: bool,
        input_permission: bool,
        capture_fixture: CaptureFixture,
        geometry: ScreenGeometry,
    }

    impl FakeBackend {
        fn new() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                fail_on: None,
                capture_permission: true,
                input_permission: true,
                capture_fixture: CaptureFixture::Valid,
                geometry: ScreenGeometry {
                    pixel_width: 8,
                    pixel_height: 6,
                    logical_x: 0.0,
                    logical_y: 0.0,
                    logical_width: 8.0,
                    logical_height: 6.0,
                },
            }
        }

        fn record(&self, call: impl Into<String>) -> std::result::Result<(), DesktopError> {
            let call = call.into();
            self.calls.lock().unwrap().push(call.clone());
            if self.fail_on.as_deref() == Some(&call) {
                Err(DesktopError::input(format!("fake failure at {call}")))
            } else {
                Ok(())
            }
        }
    }

    impl DesktopBackend for FakeBackend {
        fn geometry(&self) -> std::result::Result<ScreenGeometry, DesktopError> {
            Ok(self.geometry)
        }

        fn preflight_capture(&self) -> std::result::Result<(), DesktopError> {
            self.capture_permission
                .then_some(())
                .ok_or_else(DesktopError::screen_permission)
        }

        fn preflight_input(&self) -> std::result::Result<(), DesktopError> {
            self.input_permission
                .then_some(())
                .ok_or_else(DesktopError::accessibility_permission)
        }

        fn capture_full(&mut self, destination: &Path) -> std::result::Result<(), DesktopError> {
            self.record("capture")?;
            if matches!(self.capture_fixture, CaptureFixture::Empty) {
                return File::create(destination)
                    .map(|_| ())
                    .map_err(|error| DesktopError::capture(error.to_string()));
            }
            let (width, height) = if matches!(self.capture_fixture, CaptureFixture::WrongSize) {
                (7, 6)
            } else {
                (8, 6)
            };
            let transparent = matches!(self.capture_fixture, CaptureFixture::Transparent);
            let image = ImageBuffer::from_fn(width, height, |x, y| {
                Rgba([
                    x as u8,
                    y as u8,
                    (x + y) as u8,
                    if transparent { 0 } else { 255 },
                ])
            });
            DynamicImage::ImageRgba8(image)
                .save_with_format(destination, ImageFormat::Png)
                .map_err(|error| DesktopError::capture(error.to_string()))
        }

        fn delay(&mut self, duration: Duration) -> std::result::Result<(), DesktopError> {
            self.record(format!("delay:{}", duration.as_millis()))
        }

        fn mouse_move(
            &mut self,
            x: u32,
            y: u32,
            held: &[MouseButton],
        ) -> std::result::Result<(), DesktopError> {
            self.record(format!("move:{x}:{y}:{}", held.len()))
        }

        fn mouse_down(&mut self, button: MouseButton) -> std::result::Result<(), DesktopError> {
            self.record(format!("mouse_down:{}", button.name()))
        }

        fn mouse_up(&mut self, button: MouseButton) -> std::result::Result<(), DesktopError> {
            self.record(format!("mouse_up:{}", button.name()))
        }

        fn mouse_wheel(
            &mut self,
            delta_x: i32,
            delta_y: i32,
        ) -> std::result::Result<(), DesktopError> {
            self.record(format!("wheel:{delta_x}:{delta_y}"))
        }

        fn key_down(&mut self, key: &str) -> std::result::Result<(), DesktopError> {
            self.record(format!("key_down:{key}"))
        }

        fn key_up(&mut self, key: &str) -> std::result::Result<(), DesktopError> {
            self.record(format!("key_up:{key}"))
        }

        fn text_input(&mut self, text: &str) -> std::result::Result<(), DesktopError> {
            self.record(format!("text:{text}"))
        }
    }

    fn parsed(value: Value) -> PlayInput {
        parse_and_validate(value).unwrap()
    }

    fn temporary_workspace(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "me-desktop-{name}-{}-{}",
            std::process::id(),
            CAPTURE_NONCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn metadata_exposes_only_play_on_supported_desktop_hosts() {
        let request = WorkerRequest {
            id: 1,
            cmd: "getTools".into(),
            tool: None,
            input: Value::Null,
        };
        let response = metadata_response(&request);
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        assert_eq!(response["output"], json!(["Play"]));
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert_eq!(response["output"], json!([]));
        assert_eq!(
            input_schema("Play").unwrap()["properties"]["operations"]["minItems"],
            1
        );
    }

    #[test]
    fn capture_is_optional_but_unique_and_final() {
        assert!(parse_and_validate(json!({"operations":[{"kind":"key_click","key":"a"}]})).is_ok());
        assert!(parse_and_validate(json!({"operations":[{"kind":"capture"}]})).is_ok());
        assert!(
            parse_and_validate(
                json!({"operations":[{"kind":"delay","delay_ms":1},{"kind":"capture"}]})
            )
            .is_ok()
        );
        let error = parse_and_validate(
            json!({"operations":[{"kind":"capture"},{"kind":"key_click","key":"a"}]}),
        )
        .unwrap_err();
        assert_eq!(error.code, "capture_must_be_final");
        let error =
            parse_and_validate(json!({"operations":[{"kind":"capture"},{"kind":"capture"}]}))
                .unwrap_err();
        assert_eq!(error.code, "multiple_captures");
    }

    #[test]
    fn complete_shape_and_static_validation_precedes_execution() {
        for value in [
            json!({"operations":[]}),
            json!({"operations":[{"kind":"unknown"}]}),
            json!({"operations":[{"kind":"delay","delay_ms":1,"extra":true}]}),
            json!({"operations":[{"kind":"capture","clip":{"x":0,"y":0,"width":0,"height":1}}]}),
            json!({"operations":[{"kind":"key_click","key":"🙂"}]}),
            json!({"operations":[{"kind":"mouse_wheel","delta_x":0,"delta_y":0}]}),
        ] {
            assert!(parse_and_validate(value).is_err());
        }
    }

    #[test]
    fn operations_execute_in_order_and_leftovers_release_after_capture() {
        let workspace = temporary_workspace("order");
        fs::create_dir_all(&workspace).unwrap();
        let input = parsed(json!({"operations":[
            {"kind":"key_down","key":"A"},
            {"kind":"delay","delay_ms":10},
            {"kind":"capture"}
        ]}));
        let mut backend = FakeBackend::new();
        let calls = Arc::clone(&backend.calls);
        let geometry = backend.geometry;
        let result = execute_play(&input, &workspace, &mut backend, Some(geometry)).unwrap();
        assert_eq!(result.state, "succeeded");
        assert_eq!(result.completed_operations, 3);
        assert_eq!(result.auto_released, vec!["key:a"]);
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["key_down:a", "delay:10", "capture", "key_up:a"]
        );
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn runtime_failure_stops_suffix_and_releases_pressed_input() {
        let workspace = temporary_workspace("failure");
        fs::create_dir_all(&workspace).unwrap();
        let input = parsed(json!({"operations":[
            {"kind":"mouse_down"},
            {"kind":"key_down","key":"b"},
            {"kind":"text_input","text":"never"}
        ]}));
        let mut backend = FakeBackend::new();
        backend.fail_on = Some("key_down:b".into());
        let calls = Arc::clone(&backend.calls);
        let result = execute_play(&input, &workspace, &mut backend, None).unwrap();
        assert_eq!(result.state, "failed");
        assert_eq!(result.completed_operations, 1);
        assert_eq!(result.failed_operation_index, Some(1));
        assert_eq!(result.auto_released, vec!["mouse:left"]);
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["mouse_down:left", "key_down:b", "mouse_up:left"]
        );
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn clip_is_strict_and_crop_is_not_scaled() {
        let workspace = temporary_workspace("clip");
        fs::create_dir_all(&workspace).unwrap();
        let input = parsed(
            json!({"operations":[{"kind":"capture","clip":{"x":2,"y":1,"width":3,"height":2}}]}),
        );
        let mut backend = FakeBackend::new();
        validate_geometry(&input, backend.geometry).unwrap();
        let geometry = backend.geometry;
        let result = execute_play(&input, &workspace, &mut backend, Some(geometry)).unwrap();
        let capture = &result.captures[0];
        let file_name = Path::new(&capture.path)
            .file_name()
            .unwrap()
            .to_string_lossy();
        let id = file_name
            .strip_prefix("capture-")
            .and_then(|value| value.strip_suffix(".png"))
            .unwrap();
        assert_eq!(id.len(), 6);
        assert!(
            id.bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        assert_eq!((capture.width, capture.height), (3, 2));
        assert_eq!((capture.full_width, capture.full_height), (8, 6));
        let image = image::open(workspace.join(&capture.path)).unwrap();
        assert_eq!((image.width(), image.height()), (3, 2));
        assert_eq!(image.to_rgba8().get_pixel(0, 0).0, [2, 1, 3, 255]);
        let second_result = execute_play(&input, &workspace, &mut backend, Some(geometry)).unwrap();
        let second_capture = &second_result.captures[0];
        assert_ne!(capture.path, second_capture.path);
        assert!(workspace.join(&second_capture.path).is_file());
        assert!(
            fs::read_dir(workspace.join(DESKTOP_TEMP_DIRECTORY))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("full-"))
        );
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn out_of_bounds_geometry_rejects_before_backend_calls() {
        let input = parsed(json!({"operations":[{"kind":"mouse_move","x":8,"y":0}]}));
        let backend = FakeBackend::new();
        assert!(validate_geometry(&input, backend.geometry).is_err());
        assert!(backend.calls.lock().unwrap().is_empty());

        let input = parsed(
            json!({"operations":[{"kind":"capture","clip":{"x":7,"y":0,"width":2,"height":1}}]}),
        );
        assert!(validate_geometry(&input, backend.geometry).is_err());
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn permission_preflights_stop_before_any_desktop_operation() {
        let workspace = temporary_workspace("permissions");
        fs::create_dir_all(&workspace).unwrap();

        let capture = parsed(json!({"operations":[{"kind":"capture"}]}));
        let mut backend = FakeBackend::new();
        backend.capture_permission = false;
        let error = execute_validated(&capture, &workspace, &mut backend).unwrap_err();
        assert_eq!(error.code, "screen_permission_required");
        assert!(error.tip.unwrap().contains("Stop using Desktop"));
        assert!(backend.calls.lock().unwrap().is_empty());

        let input = parsed(json!({"operations":[{"kind":"mouse_move","x":1,"y":1}]}));
        let mut backend = FakeBackend::new();
        backend.input_permission = false;
        let error = execute_validated(&input, &workspace, &mut backend).unwrap_err();
        assert_eq!(error.code, "accessibility_permission_required");
        assert!(error.tip.unwrap().contains("Stop using Desktop"));
        assert!(backend.calls.lock().unwrap().is_empty());
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn empty_transparent_and_wrong_size_captures_never_succeed() {
        for fixture in [
            CaptureFixture::Empty,
            CaptureFixture::Transparent,
            CaptureFixture::WrongSize,
        ] {
            let workspace = temporary_workspace("bad-capture");
            fs::create_dir_all(&workspace).unwrap();
            let input = parsed(json!({"operations":[{"kind":"capture"}]}));
            let mut backend = FakeBackend::new();
            backend.capture_fixture = fixture;
            let result = execute_validated(&input, &workspace, &mut backend).unwrap();
            assert_eq!(result.state, "failed");
            assert_eq!(result.completed_operations, 0);
            assert_eq!(result.failed_operation_index, Some(0));
            assert_eq!(result.error.unwrap().code, "desktop_capture_failed");
            assert!(result.captures.is_empty());
            let capture_directory = workspace.join(DESKTOP_TEMP_DIRECTORY);
            assert!(
                !capture_directory.exists()
                    || fs::read_dir(capture_directory).unwrap().next().is_none()
            );
            fs::remove_dir_all(workspace).unwrap();
        }
    }

    #[test]
    fn desktop_lease_is_host_global_and_nonblocking() {
        let directory = temporary_workspace("lease");
        let first = DesktopLease::acquire_at(&directory).unwrap();
        let error = DesktopLease::acquire_at(&directory).unwrap_err();
        assert_eq!(error.code, "desktop_busy");
        drop(first);
        DesktopLease::acquire_at(&directory).unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn worker_metadata_uses_jsonl_result_frames() {
        let input = [
            json!({"id":1,"cmd":"getTools"}),
            json!({"id":2,"cmd":"getInputSchema","tool":"Play"}),
            json!({"id":3,"cmd":"getOutputSchema","tool":"Play"}),
            json!({"id":4,"cmd":"getInstructions","tool":"Play"}),
        ]
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let mut output = Vec::new();
        run(input.as_bytes(), &mut output, Path::new(".")).unwrap();
        let frames = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(frames.len(), 4);
        assert!(frames.iter().all(|frame| frame["type"] == "result"));
        assert!(
            frames[3]["output"]
                .as_str()
                .unwrap()
                .contains("screen_permission_required")
        );
    }

    #[test]
    fn supported_keys_are_normalized_without_accepting_unicode_text() {
        let input = parsed(json!({"operations":[
            {"kind":"key_click","key":"CMD"},
            {"kind":"key_click","key":"Arrow-Left"},
            {"kind":"key_click","key":"F12"},
            {"kind":"key_click","key":"/"}
        ]}));
        let keys = input
            .operations
            .iter()
            .filter_map(|operation| match operation {
                Operation::KeyClick { key } => Some(key.as_str()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(keys, BTreeSet::from(["command", "f12", "left", "slash"]));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_rejects_hardware_function_key_during_static_validation() {
        let error = parse_and_validate(json!({
            "operations": [
                {"kind":"mouse_down"},
                {"kind":"key_click","key":"fn"}
            ]
        }))
        .unwrap_err();
        assert_eq!(error.code, "invalid_arguments");
        assert!(
            error
                .message
                .contains("not supported on this host platform")
        );
    }
}
