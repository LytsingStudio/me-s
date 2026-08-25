use std::{
    ffi::c_void,
    ptr,
    time::{Duration, Instant},
};

use image::RgbaImage;

use super::{
    NativeRemoteControlBackend, RemoteControlError, RemoteMouseButton, RemoteResult, ScreenGeometry,
};

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
const IMAGE_ALPHA_PREMULTIPLIED_LAST: u32 = 1;
const BITMAP_BYTE_ORDER_32_BIG: u32 = 4 << 12;
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
    fn CGDisplayCreateImage(display: u32) -> *mut c_void;
    fn CGImageGetWidth(image: *const c_void) -> usize;
    fn CGImageGetHeight(image: *const c_void) -> usize;
    fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        color_space: *const c_void,
        bitmap_info: u32,
    ) -> *mut c_void;
    fn CGContextDrawImage(context: *mut c_void, rect: CGRect, image: *const c_void);
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

pub(super) struct MacRemoteControlBackend {
    geometry: Option<ScreenGeometry>,
    last_mouse_point: Option<CGPoint>,
    last_clicks: [Option<LastClick>; 3],
    active_click_counts: [i64; 3],
}

impl MacRemoteControlBackend {
    pub(super) fn new() -> Self {
        Self {
            geometry: None,
            last_mouse_point: None,
            last_clicks: [None, None, None],
            active_click_counts: [1, 1, 1],
        }
    }

    fn screen_permission(&self) -> RemoteResult<()> {
        if unsafe { CGPreflightScreenCaptureAccess() } {
            Ok(())
        } else {
            Err(RemoteControlError::screen_permission())
        }
    }

    fn input_permission(&self) -> RemoteResult<()> {
        if unsafe { AXIsProcessTrusted() } != 0 {
            Ok(())
        } else {
            Err(RemoteControlError::accessibility_permission())
        }
    }

    fn current_geometry(&mut self) -> RemoteResult<ScreenGeometry> {
        let display = unsafe { CGMainDisplayID() };
        let image = unsafe { CGDisplayCreateImage(display) };
        if image.is_null() {
            self.screen_permission()?;
            return Err(RemoteControlError::capture(
                "CoreGraphics could not capture the primary display",
            ));
        }
        let width = unsafe { CGImageGetWidth(image) };
        let height = unsafe { CGImageGetHeight(image) };
        unsafe { CFRelease(image.cast_const()) };
        let geometry = geometry(display, width, height)?;
        self.geometry = Some(geometry);
        Ok(geometry)
    }

    fn logical_point(&self, x: u32, y: u32) -> RemoteResult<CGPoint> {
        let geometry = self.geometry.ok_or_else(|| {
            RemoteControlError::input("remote-control display geometry is unavailable")
        })?;
        Ok(CGPoint {
            x: geometry.logical_x
                + f64::from(x) * geometry.logical_width / f64::from(geometry.pixel_width),
            y: geometry.logical_y
                + f64::from(y) * geometry.logical_height / f64::from(geometry.pixel_height),
        })
    }

    fn current_point(&self) -> RemoteResult<CGPoint> {
        let event = unsafe { CGEventCreate(ptr::null()) };
        if event.is_null() {
            return Err(RemoteControlError::input(
                "CoreGraphics could not read the current mouse location",
            ));
        }
        let point = unsafe { CGEventGetLocation(event) };
        unsafe { CFRelease(event.cast_const()) };
        if !point.x.is_finite() || !point.y.is_finite() {
            return Err(RemoteControlError::input(
                "CoreGraphics returned an invalid mouse location",
            ));
        }
        Ok(point)
    }

    fn post_mouse(
        &mut self,
        event_type: u32,
        point: CGPoint,
        button: RemoteMouseButton,
        click_count: Option<i64>,
    ) -> RemoteResult<()> {
        let event = unsafe {
            CGEventCreateMouseEvent(ptr::null(), event_type, point, mouse_button_code(button))
        };
        if event.is_null() {
            return Err(RemoteControlError::input(
                "CoreGraphics could not create a remote mouse event",
            ));
        }
        if let Some(click_count) = click_count {
            unsafe { CGEventSetIntegerValueField(event, MOUSE_EVENT_CLICK_STATE, click_count) };
        }
        unsafe {
            CGEventPost(EVENT_TAP_HID, event);
            CFRelease(event.cast_const());
        }
        Ok(())
    }

    fn click_count(&self, button: RemoteMouseButton, point: CGPoint) -> i64 {
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

    fn post_key(&self, key: &str, down: bool) -> RemoteResult<()> {
        let code = key_code(key).ok_or_else(|| {
            RemoteControlError::input(format!("macOS has no remote key mapping for {key:?}"))
        })?;
        let event = unsafe { CGEventCreateKeyboardEvent(ptr::null(), code, down) };
        if event.is_null() {
            return Err(RemoteControlError::input(
                "CoreGraphics could not create a remote keyboard event",
            ));
        }
        unsafe {
            CGEventPost(EVENT_TAP_HID, event);
            CFRelease(event.cast_const());
        }
        Ok(())
    }
}

impl NativeRemoteControlBackend for MacRemoteControlBackend {
    fn preflight_control(&mut self) -> RemoteResult<ScreenGeometry> {
        self.screen_permission()?;
        self.input_permission()?;
        self.current_geometry()
    }

    fn capture(&mut self) -> RemoteResult<(ScreenGeometry, RgbaImage)> {
        self.screen_permission()?;
        let display = unsafe { CGMainDisplayID() };
        let image = unsafe { CGDisplayCreateImage(display) };
        if image.is_null() {
            self.screen_permission()?;
            return Err(RemoteControlError::capture(
                "CoreGraphics could not capture the primary display",
            ));
        }
        let image = CoreFoundationObject(image);
        let width = unsafe { CGImageGetWidth(image.0) };
        let height = unsafe { CGImageGetHeight(image.0) };
        let geometry = geometry(display, width, height)?;
        let bytes_per_row = width
            .checked_mul(4)
            .ok_or_else(|| RemoteControlError::capture("macOS screenshot row size overflowed"))?;
        let byte_count = bytes_per_row.checked_mul(height).ok_or_else(|| {
            RemoteControlError::capture("macOS screenshot buffer size overflowed")
        })?;
        let mut pixels = vec![0_u8; byte_count];
        let color_space = unsafe { CGColorSpaceCreateDeviceRGB() };
        if color_space.is_null() {
            return Err(RemoteControlError::capture(
                "CoreGraphics could not create an RGB color space",
            ));
        }
        let color_space = CoreFoundationObject(color_space);
        let context = unsafe {
            CGBitmapContextCreate(
                pixels.as_mut_ptr().cast(),
                width,
                height,
                8,
                bytes_per_row,
                color_space.0,
                IMAGE_ALPHA_PREMULTIPLIED_LAST | BITMAP_BYTE_ORDER_32_BIG,
            )
        };
        if context.is_null() {
            return Err(RemoteControlError::capture(
                "CoreGraphics could not create a remote screenshot bitmap",
            ));
        }
        let context = CoreFoundationObject(context);
        unsafe {
            CGContextDrawImage(
                context.0,
                CGRect {
                    origin: CGPoint { x: 0.0, y: 0.0 },
                    size: CGSize {
                        width: width as f64,
                        height: height as f64,
                    },
                },
                image.0,
            );
        }
        for pixel in pixels.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        let width = u32::try_from(width)
            .map_err(|_| RemoteControlError::capture("macOS display width is too large"))?;
        let height = u32::try_from(height)
            .map_err(|_| RemoteControlError::capture("macOS display height is too large"))?;
        let image = RgbaImage::from_raw(width, height, pixels).ok_or_else(|| {
            RemoteControlError::capture("CoreGraphics returned invalid remote screenshot pixels")
        })?;
        self.geometry = Some(geometry);
        Ok((geometry, image))
    }

    fn mouse_move(&mut self, x: u32, y: u32, held: &[RemoteMouseButton]) -> RemoteResult<()> {
        self.input_permission()?;
        let point = self.logical_point(x, y)?;
        let (event_type, button) = if held.contains(&RemoteMouseButton::Left) {
            (EVENT_LEFT_MOUSE_DRAGGED, RemoteMouseButton::Left)
        } else if held.contains(&RemoteMouseButton::Right) {
            (EVENT_RIGHT_MOUSE_DRAGGED, RemoteMouseButton::Right)
        } else if held.contains(&RemoteMouseButton::Middle) {
            (EVENT_OTHER_MOUSE_DRAGGED, RemoteMouseButton::Middle)
        } else {
            (EVENT_MOUSE_MOVED, RemoteMouseButton::Left)
        };
        self.post_mouse(event_type, point, button, None)?;
        self.last_mouse_point = Some(point);
        Ok(())
    }

    fn mouse_down(&mut self, button: RemoteMouseButton) -> RemoteResult<()> {
        self.input_permission()?;
        let point = match self.last_mouse_point {
            Some(point) => point,
            None => self.current_point()?,
        };
        let count = self.click_count(button, point);
        self.active_click_counts[button_index(button)] = count;
        self.post_mouse(mouse_down_type(button), point, button, Some(count))?;
        self.last_mouse_point = Some(point);
        Ok(())
    }

    fn mouse_up(&mut self, button: RemoteMouseButton) -> RemoteResult<()> {
        self.input_permission()?;
        let point = match self.last_mouse_point {
            Some(point) => point,
            None => self.current_point()?,
        };
        let index = button_index(button);
        let count = self.active_click_counts[index];
        self.post_mouse(mouse_up_type(button), point, button, Some(count))?;
        self.last_mouse_point = Some(point);
        self.last_clicks[index] = Some(LastClick {
            at: Instant::now(),
            point,
        });
        Ok(())
    }

    fn mouse_wheel(&mut self, delta_x: i32, delta_y: i32) -> RemoteResult<()> {
        self.input_permission()?;
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
            return Err(RemoteControlError::input(
                "CoreGraphics could not create a remote scroll event",
            ));
        }
        unsafe {
            CGEventPost(EVENT_TAP_HID, event);
            CFRelease(event.cast_const());
        }
        Ok(())
    }

    fn key_down(&mut self, key: &str) -> RemoteResult<()> {
        self.input_permission()?;
        self.post_key(key, true)
    }

    fn key_up(&mut self, key: &str) -> RemoteResult<()> {
        self.input_permission()?;
        self.post_key(key, false)
    }

    fn text(&mut self, text: &str) -> RemoteResult<()> {
        self.input_permission()?;
        for character in text.chars() {
            let mut buffer = [0_u16; 2];
            let encoded = character.encode_utf16(&mut buffer);
            for down in [true, false] {
                let event = unsafe { CGEventCreateKeyboardEvent(ptr::null(), 0, down) };
                if event.is_null() {
                    return Err(RemoteControlError::input(
                        "CoreGraphics could not create a remote Unicode event",
                    ));
                }
                unsafe {
                    CGEventKeyboardSetUnicodeString(event, encoded.len(), encoded.as_ptr());
                    CGEventPost(EVENT_TAP_HID, event);
                    CFRelease(event.cast_const());
                }
            }
        }
        Ok(())
    }
}

fn geometry(display: u32, width: usize, height: usize) -> RemoteResult<ScreenGeometry> {
    let bounds = unsafe { CGDisplayBounds(display) };
    let pixel_width = u32::try_from(width)
        .map_err(|_| RemoteControlError::capture("macOS display width is too large"))?;
    let pixel_height = u32::try_from(height)
        .map_err(|_| RemoteControlError::capture("macOS display height is too large"))?;
    if pixel_width == 0
        || pixel_height == 0
        || !bounds.origin.x.is_finite()
        || !bounds.origin.y.is_finite()
        || !bounds.size.width.is_finite()
        || !bounds.size.height.is_finite()
        || bounds.size.width <= 0.0
        || bounds.size.height <= 0.0
    {
        return Err(RemoteControlError::capture(
            "CoreGraphics returned invalid primary-display geometry",
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

struct CoreFoundationObject(*mut c_void);

impl Drop for CoreFoundationObject {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0.cast_const()) };
    }
}

fn button_index(button: RemoteMouseButton) -> usize {
    match button {
        RemoteMouseButton::Left => 0,
        RemoteMouseButton::Right => 1,
        RemoteMouseButton::Middle => 2,
    }
}

fn mouse_button_code(button: RemoteMouseButton) -> u32 {
    button_index(button) as u32
}

fn mouse_down_type(button: RemoteMouseButton) -> u32 {
    match button {
        RemoteMouseButton::Left => EVENT_LEFT_MOUSE_DOWN,
        RemoteMouseButton::Right => EVENT_RIGHT_MOUSE_DOWN,
        RemoteMouseButton::Middle => EVENT_OTHER_MOUSE_DOWN,
    }
}

fn mouse_up_type(button: RemoteMouseButton) -> u32 {
    match button {
        RemoteMouseButton::Left => EVENT_LEFT_MOUSE_UP,
        RemoteMouseButton::Right => EVENT_RIGHT_MOUSE_UP,
        RemoteMouseButton::Middle => EVENT_OTHER_MOUSE_UP,
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
    fn all_remote_dom_keys_have_macos_codes() {
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
            assert!(
                key_code(key).is_some(),
                "missing macOS remote key mapping for {key}"
            );
        }
        for byte in b'a'..=b'z' {
            assert!(key_code(&char::from(byte).to_string()).is_some());
        }
        for byte in b'0'..=b'9' {
            assert!(key_code(&char::from(byte).to_string()).is_some());
        }
        for number in 1..=20 {
            assert!(key_code(&format!("f{number}")).is_some());
        }
    }
}
