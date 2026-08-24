use std::{
    ffi::c_void,
    fs,
    path::Path,
    process::Command,
    ptr,
    time::{Duration, Instant},
};

use super::{DesktopBackend, DesktopError, MouseButton, ScreenGeometry};

const EVENT_TAP_HID: u32 = 0;
const EVENT_LEFT_MOUSE_DOWN: u32 = 1;
const EVENT_LEFT_MOUSE_UP: u32 = 2;
const EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
const EVENT_RIGHT_MOUSE_UP: u32 = 4;
const EVENT_MOUSE_MOVED: u32 = 5;
const EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
const EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
const EVENT_OTHER_MOUSE_DOWN: u32 = 25;
const EVENT_OTHER_MOUSE_UP: u32 = 26;
const EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;
const SCROLL_EVENT_UNIT_LINE: u32 = 1;
const MOUSE_EVENT_CLICK_STATE: u32 = 1;
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(600);
const DOUBLE_CLICK_DISTANCE_SQUARED: f64 = 36.0;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGDisplayCopyDisplayMode(display: u32) -> *const c_void;
    fn CGDisplayModeGetPixelWidth(mode: *const c_void) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: *const c_void) -> usize;
    fn CGEventCreate(source: *const c_void) -> *mut c_void;
    fn CGEventGetLocation(event: *const c_void) -> CGPoint;
    fn CGEventCreateMouseEvent(
        source: *const c_void,
        mouse_type: u32,
        mouse_cursor_position: CGPoint,
        mouse_button: u32,
    ) -> *mut c_void;
    fn CGEventSetIntegerValueField(event: *mut c_void, field: u32, value: i64);
    fn CGEventCreateScrollWheelEvent2(
        source: *const c_void,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> *mut c_void;
    fn CGEventCreateKeyboardEvent(
        source: *const c_void,
        virtual_key: u16,
        key_down: bool,
    ) -> *mut c_void;
    fn CGEventKeyboardSetUnicodeString(
        event: *mut c_void,
        string_length: usize,
        unicode_string: *const u16,
    );
    fn CGEventPost(tap: u32, event: *const c_void);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(value: *const c_void);
}

#[derive(Clone, Copy)]
struct LastClick {
    at: Instant,
    point: CGPoint,
}

pub(super) struct MacDesktopBackend {
    geometry: ScreenGeometry,
    last_clicks: [Option<LastClick>; 3],
    active_click_counts: [i64; 3],
}

impl MacDesktopBackend {
    pub(super) fn new() -> std::result::Result<Self, DesktopError> {
        Ok(Self {
            geometry: primary_geometry()?,
            last_clicks: [None, None, None],
            active_click_counts: [1, 1, 1],
        })
    }

    fn logical_point(&self, x: u32, y: u32) -> CGPoint {
        CGPoint {
            x: self.geometry.logical_x
                + f64::from(x) * self.geometry.logical_width / f64::from(self.geometry.pixel_width),
            y: self.geometry.logical_y
                + f64::from(y) * self.geometry.logical_height
                    / f64::from(self.geometry.pixel_height),
        }
    }

    fn current_point(&self) -> std::result::Result<CGPoint, DesktopError> {
        let event = unsafe { CGEventCreate(ptr::null()) };
        if event.is_null() {
            return Err(DesktopError::input(
                "CoreGraphics could not read the current mouse location",
            ));
        }
        let point = unsafe { CGEventGetLocation(event) };
        unsafe { CFRelease(event.cast_const()) };
        if !point.x.is_finite() || !point.y.is_finite() {
            return Err(DesktopError::input(
                "CoreGraphics returned an invalid mouse location",
            ));
        }
        Ok(point)
    }

    fn post_mouse(
        &mut self,
        event_type: u32,
        point: CGPoint,
        button: MouseButton,
        click_count: Option<i64>,
    ) -> std::result::Result<(), DesktopError> {
        let event = unsafe {
            CGEventCreateMouseEvent(ptr::null(), event_type, point, mouse_button_code(button))
        };
        if event.is_null() {
            return Err(DesktopError::input(
                "CoreGraphics could not create a mouse event",
            ));
        }
        if let Some(click_count) = click_count {
            unsafe {
                CGEventSetIntegerValueField(event, MOUSE_EVENT_CLICK_STATE, click_count);
            }
        }
        unsafe {
            CGEventPost(EVENT_TAP_HID, event);
            CFRelease(event.cast_const());
        }
        Ok(())
    }

    fn click_count(&self, button: MouseButton, point: CGPoint) -> i64 {
        let Some(last) = self.last_clicks[button_index(button)] else {
            return 1;
        };
        let dx = point.x - last.point.x;
        let dy = point.y - last.point.y;
        if last.at.elapsed() <= DOUBLE_CLICK_INTERVAL
            && dx * dx + dy * dy <= DOUBLE_CLICK_DISTANCE_SQUARED
        {
            2
        } else {
            1
        }
    }

    fn post_keyboard(&self, key: &str, down: bool) -> std::result::Result<(), DesktopError> {
        let key_code = key_code(key)
            .ok_or_else(|| DesktopError::input(format!("macOS has no key mapping for {key:?}")))?;
        let event = unsafe { CGEventCreateKeyboardEvent(ptr::null(), key_code, down) };
        if event.is_null() {
            return Err(DesktopError::input(
                "CoreGraphics could not create a keyboard event",
            ));
        }
        unsafe {
            CGEventPost(EVENT_TAP_HID, event);
            CFRelease(event.cast_const());
        }
        Ok(())
    }

    fn permission_after_capture_failure(&self, message: String) -> DesktopError {
        if unsafe { CGPreflightScreenCaptureAccess() } {
            DesktopError::capture(message)
        } else {
            DesktopError::screen_permission()
        }
    }
}

impl DesktopBackend for MacDesktopBackend {
    fn geometry(&self) -> std::result::Result<ScreenGeometry, DesktopError> {
        Ok(self.geometry)
    }

    fn preflight_capture(&self) -> std::result::Result<(), DesktopError> {
        if unsafe { CGPreflightScreenCaptureAccess() } {
            Ok(())
        } else {
            Err(DesktopError::screen_permission())
        }
    }

    fn preflight_input(&self) -> std::result::Result<(), DesktopError> {
        if unsafe { AXIsProcessTrusted() } != 0 {
            Ok(())
        } else {
            Err(DesktopError::accessibility_permission())
        }
    }

    fn capture_full(&mut self, destination: &Path) -> std::result::Result<(), DesktopError> {
        self.preflight_capture()?;
        let output = Command::new("/usr/sbin/screencapture")
            .args(["-x", "-tpng", "-D1"])
            .arg(destination)
            .output()
            .map_err(|error| {
                self.permission_after_capture_failure(format!(
                    "cannot start macOS full-screen capture: {error}"
                ))
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let detail = if stderr.is_empty() {
                format!("macOS screencapture exited with {}", output.status)
            } else {
                format!("macOS screencapture failed: {stderr}")
            };
            return Err(self.permission_after_capture_failure(detail));
        }
        if !destination.is_file() {
            return Err(self.permission_after_capture_failure(
                "macOS screencapture reported success without creating a PNG".into(),
            ));
        }
        if fs::metadata(destination).map_or(true, |metadata| metadata.len() == 0) {
            return Err(self.permission_after_capture_failure(
                "macOS screencapture produced an empty file".into(),
            ));
        }
        Ok(())
    }

    fn delay(&mut self, duration: Duration) -> std::result::Result<(), DesktopError> {
        std::thread::sleep(duration);
        Ok(())
    }

    fn mouse_move(
        &mut self,
        x: u32,
        y: u32,
        held: &[MouseButton],
    ) -> std::result::Result<(), DesktopError> {
        let point = self.logical_point(x, y);
        let (event_type, button) = if held.contains(&MouseButton::Left) {
            (EVENT_LEFT_MOUSE_DRAGGED, MouseButton::Left)
        } else if held.contains(&MouseButton::Right) {
            (EVENT_RIGHT_MOUSE_DRAGGED, MouseButton::Right)
        } else if held.contains(&MouseButton::Middle) {
            (EVENT_OTHER_MOUSE_DRAGGED, MouseButton::Middle)
        } else {
            (EVENT_MOUSE_MOVED, MouseButton::Left)
        };
        self.post_mouse(event_type, point, button, None)
    }

    fn mouse_down(&mut self, button: MouseButton) -> std::result::Result<(), DesktopError> {
        let point = self.current_point()?;
        let count = self.click_count(button, point);
        self.active_click_counts[button_index(button)] = count;
        self.post_mouse(mouse_down_type(button), point, button, Some(count))
    }

    fn mouse_up(&mut self, button: MouseButton) -> std::result::Result<(), DesktopError> {
        let point = self.current_point()?;
        let index = button_index(button);
        let count = self.active_click_counts[index];
        self.post_mouse(mouse_up_type(button), point, button, Some(count))?;
        self.last_clicks[index] = Some(LastClick {
            at: Instant::now(),
            point,
        });
        Ok(())
    }

    fn mouse_wheel(&mut self, delta_x: i32, delta_y: i32) -> std::result::Result<(), DesktopError> {
        let event = unsafe {
            CGEventCreateScrollWheelEvent2(
                ptr::null(),
                SCROLL_EVENT_UNIT_LINE,
                2,
                delta_y,
                delta_x,
                0,
            )
        };
        if event.is_null() {
            return Err(DesktopError::input(
                "CoreGraphics could not create a scroll event",
            ));
        }
        unsafe {
            CGEventPost(EVENT_TAP_HID, event);
            CFRelease(event.cast_const());
        }
        Ok(())
    }

    fn key_down(&mut self, key: &str) -> std::result::Result<(), DesktopError> {
        self.post_keyboard(key, true)
    }

    fn key_up(&mut self, key: &str) -> std::result::Result<(), DesktopError> {
        self.post_keyboard(key, false)
    }

    fn text_input(&mut self, text: &str) -> std::result::Result<(), DesktopError> {
        for character in text.chars() {
            let mut buffer = [0_u16; 2];
            let encoded = character.encode_utf16(&mut buffer);
            let down = unsafe { CGEventCreateKeyboardEvent(ptr::null(), 0, true) };
            if down.is_null() {
                return Err(DesktopError::input(
                    "CoreGraphics could not create a Unicode key-down event",
                ));
            }
            unsafe {
                CGEventKeyboardSetUnicodeString(down, encoded.len(), encoded.as_ptr());
                CGEventPost(EVENT_TAP_HID, down);
                CFRelease(down.cast_const());
            }
            let up = unsafe { CGEventCreateKeyboardEvent(ptr::null(), 0, false) };
            if up.is_null() {
                return Err(DesktopError::input(
                    "CoreGraphics could not create a Unicode key-up event",
                ));
            }
            unsafe {
                CGEventKeyboardSetUnicodeString(up, encoded.len(), encoded.as_ptr());
                CGEventPost(EVENT_TAP_HID, up);
                CFRelease(up.cast_const());
            }
        }
        Ok(())
    }
}

fn primary_geometry() -> std::result::Result<ScreenGeometry, DesktopError> {
    let display = unsafe { CGMainDisplayID() };
    let bounds = unsafe { CGDisplayBounds(display) };
    let mode = unsafe { CGDisplayCopyDisplayMode(display) };
    if mode.is_null() {
        return Err(DesktopError::capture(
            "CoreGraphics could not read the primary display mode",
        ));
    }
    let pixel_width = unsafe { CGDisplayModeGetPixelWidth(mode) };
    let pixel_height = unsafe { CGDisplayModeGetPixelHeight(mode) };
    unsafe { CFRelease(mode) };
    let pixel_width = u32::try_from(pixel_width)
        .map_err(|_| DesktopError::capture("primary display width exceeds the supported range"))?;
    let pixel_height = u32::try_from(pixel_height)
        .map_err(|_| DesktopError::capture("primary display height exceeds the supported range"))?;
    if pixel_width == 0
        || pixel_height == 0
        || !bounds.origin.x.is_finite()
        || !bounds.origin.y.is_finite()
        || !bounds.size.width.is_finite()
        || !bounds.size.height.is_finite()
        || bounds.size.width <= 0.0
        || bounds.size.height <= 0.0
    {
        return Err(DesktopError::capture(
            "CoreGraphics returned invalid primary display geometry",
        ));
    }
    Ok(ScreenGeometry {
        pixel_width,
        pixel_height,
        logical_x: bounds.origin.x,
        logical_y: bounds.origin.y,
        logical_width: bounds.size.width,
        logical_height: bounds.size.height,
    })
}

fn button_index(button: MouseButton) -> usize {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
    }
}

fn mouse_button_code(button: MouseButton) -> u32 {
    button_index(button) as u32
}

fn mouse_down_type(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => EVENT_LEFT_MOUSE_DOWN,
        MouseButton::Right => EVENT_RIGHT_MOUSE_DOWN,
        MouseButton::Middle => EVENT_OTHER_MOUSE_DOWN,
    }
}

fn mouse_up_type(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => EVENT_LEFT_MOUSE_UP,
        MouseButton::Right => EVENT_RIGHT_MOUSE_UP,
        MouseButton::Middle => EVENT_OTHER_MOUSE_UP,
    }
}

fn key_code(key: &str) -> Option<u16> {
    Some(match key {
        "a" => 0,
        "s" => 1,
        "d" => 2,
        "f" => 3,
        "h" => 4,
        "g" => 5,
        "z" => 6,
        "x" => 7,
        "c" => 8,
        "v" => 9,
        "b" => 11,
        "q" => 12,
        "w" => 13,
        "e" => 14,
        "r" => 15,
        "y" => 16,
        "t" => 17,
        "1" => 18,
        "2" => 19,
        "3" => 20,
        "4" => 21,
        "6" => 22,
        "5" => 23,
        "equal" => 24,
        "9" => 25,
        "7" => 26,
        "minus" => 27,
        "8" => 28,
        "0" => 29,
        "right_bracket" => 30,
        "o" => 31,
        "u" => 32,
        "left_bracket" => 33,
        "i" => 34,
        "p" => 35,
        "return" => 36,
        "l" => 37,
        "j" => 38,
        "quote" => 39,
        "k" => 40,
        "semicolon" => 41,
        "backslash" => 42,
        "comma" => 43,
        "slash" => 44,
        "n" => 45,
        "m" => 46,
        "period" => 47,
        "tab" => 48,
        "space" => 49,
        "grave" => 50,
        "backspace" => 51,
        "escape" => 53,
        "right_command" => 54,
        "command" => 55,
        "shift" => 56,
        "caps_lock" => 57,
        "option" => 58,
        "control" => 59,
        "right_shift" => 60,
        "right_option" => 61,
        "right_control" => 62,
        "function" => 63,
        "f17" => 64,
        "f18" => 79,
        "f19" => 80,
        "f20" => 90,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f3" => 99,
        "f8" => 100,
        "f9" => 101,
        "f11" => 103,
        "f13" => 105,
        "f16" => 106,
        "f14" => 107,
        "f10" => 109,
        "f12" => 111,
        "f15" => 113,
        "home" => 115,
        "page_up" => 116,
        "delete" => 117,
        "f4" => 118,
        "end" => 119,
        "f2" => 120,
        "page_down" => 121,
        "f1" => 122,
        "left" => 123,
        "right" => 124,
        "down" => 125,
        "up" => 126,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_normalized_named_key_has_a_macos_mapping() {
        for key in [
            "return",
            "escape",
            "space",
            "tab",
            "backspace",
            "delete",
            "left",
            "right",
            "up",
            "down",
            "home",
            "end",
            "page_up",
            "page_down",
            "shift",
            "right_shift",
            "control",
            "right_control",
            "option",
            "right_option",
            "command",
            "right_command",
            "caps_lock",
            "function",
            "minus",
            "equal",
            "left_bracket",
            "right_bracket",
            "backslash",
            "semicolon",
            "quote",
            "comma",
            "period",
            "slash",
            "grave",
        ] {
            assert!(key_code(key).is_some(), "missing mapping for {key}");
        }
        for number in 1..=20 {
            assert!(key_code(&format!("f{number}")).is_some());
        }
    }
}
