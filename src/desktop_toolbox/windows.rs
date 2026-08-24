use std::{ffi::c_void, fs::OpenOptions, mem::size_of, path::Path, ptr, time::Duration};

use image::{DynamicImage, ImageFormat, RgbaImage};

use super::{DesktopBackend, DesktopError, MouseButton, ScreenGeometry};

type Handle = isize;
type HBitmap = Handle;
type Hdc = Handle;
type HDesktop = Handle;
type HGdiObject = Handle;
type HWindowStation = Handle;

const SM_CXSCREEN: i32 = 0;
const SM_CYSCREEN: i32 = 1;
const UOI_NAME: i32 = 2;
const DESKTOP_READOBJECTS: u32 = 0x0001;
const DESKTOP_SWITCHDESKTOP: u32 = 0x0100;

const SRCCOPY: u32 = 0x00cc_0020;
const CAPTUREBLT: u32 = 0x4000_0000;
const BI_RGB: u32 = 0;
const DIB_RGB_COLORS: u32 = 0;
const HGDI_ERROR: HGdiObject = -1_isize;

const INPUT_MOUSE: u32 = 0;
const INPUT_KEYBOARD: u32 = 1;
const MOUSEEVENTF_MOVE: u32 = 0x0001;
const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
const MOUSEEVENTF_WHEEL: u32 = 0x0800;
const MOUSEEVENTF_HWHEEL: u32 = 0x1000;
const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;
const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_UNICODE: u32 = 0x0004;
const WHEEL_DELTA: i32 = 120;

const VK_BACK: u16 = 0x08;
const VK_TAB: u16 = 0x09;
const VK_RETURN: u16 = 0x0d;
const VK_CAPITAL: u16 = 0x14;
const VK_ESCAPE: u16 = 0x1b;
const VK_SPACE: u16 = 0x20;
const VK_PRIOR: u16 = 0x21;
const VK_NEXT: u16 = 0x22;
const VK_END: u16 = 0x23;
const VK_HOME: u16 = 0x24;
const VK_LEFT: u16 = 0x25;
const VK_UP: u16 = 0x26;
const VK_RIGHT: u16 = 0x27;
const VK_DOWN: u16 = 0x28;
const VK_DELETE: u16 = 0x2e;
const VK_LWIN: u16 = 0x5b;
const VK_RWIN: u16 = 0x5c;
const VK_F1: u16 = 0x70;
const VK_LSHIFT: u16 = 0xa0;
const VK_RSHIFT: u16 = 0xa1;
const VK_LCONTROL: u16 = 0xa2;
const VK_RCONTROL: u16 = 0xa3;
const VK_LMENU: u16 = 0xa4;
const VK_RMENU: u16 = 0xa5;
const VK_OEM_1: u16 = 0xba;
const VK_OEM_PLUS: u16 = 0xbb;
const VK_OEM_COMMA: u16 = 0xbc;
const VK_OEM_MINUS: u16 = 0xbd;
const VK_OEM_PERIOD: u16 = 0xbe;
const VK_OEM_2: u16 = 0xbf;
const VK_OEM_3: u16 = 0xc0;
const VK_OEM_4: u16 = 0xdb;
const VK_OEM_5: u16 = 0xdc;
const VK_OEM_6: u16 = 0xdd;
const VK_OEM_7: u16 = 0xde;

#[repr(C)]
#[derive(Clone, Copy)]
struct BitmapInfoHeader {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_pixels_per_meter: i32,
    y_pixels_per_meter: i32,
    colors_used: u32,
    colors_important: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RgbQuad {
    blue: u8,
    green: u8,
    red: u8,
    reserved: u8,
}

#[repr(C)]
struct BitmapInfo {
    header: BitmapInfoHeader,
    colors: [RgbQuad; 1],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MouseInput {
    dx: i32,
    dy: i32,
    mouse_data: u32,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KeyboardInput {
    virtual_key: u16,
    scan_code: u16,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct HardwareInput {
    message: u32,
    parameter_low: u16,
    parameter_high: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
union InputData {
    mouse: MouseInput,
    keyboard: KeyboardInput,
    hardware: HardwareInput,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Input {
    input_type: u32,
    data: InputData,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KeyMapping {
    virtual_key: u16,
    extended: bool,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn SetProcessDPIAware() -> i32;
    fn GetSystemMetrics(index: i32) -> i32;
    fn GetDC(window: Handle) -> Hdc;
    fn ReleaseDC(window: Handle, dc: Hdc) -> i32;
    fn SendInput(input_count: u32, inputs: *const Input, input_size: i32) -> u32;
    fn OpenInputDesktop(flags: u32, inherit: i32, desired_access: u32) -> HDesktop;
    fn CloseDesktop(desktop: HDesktop) -> i32;
    fn GetProcessWindowStation() -> HWindowStation;
    fn GetUserObjectInformationW(
        object: Handle,
        index: i32,
        information: *mut c_void,
        length: u32,
        needed: *mut u32,
    ) -> i32;
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn CreateCompatibleDC(dc: Hdc) -> Hdc;
    fn DeleteDC(dc: Hdc) -> i32;
    fn CreateCompatibleBitmap(dc: Hdc, width: i32, height: i32) -> HBitmap;
    fn SelectObject(dc: Hdc, object: HGdiObject) -> HGdiObject;
    fn DeleteObject(object: HGdiObject) -> i32;
    fn BitBlt(
        destination: Hdc,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        source: Hdc,
        source_x: i32,
        source_y: i32,
        operation: u32,
    ) -> i32;
    fn GetDIBits(
        dc: Hdc,
        bitmap: HBitmap,
        first_scan_line: u32,
        scan_line_count: u32,
        bits: *mut c_void,
        info: *mut BitmapInfo,
        usage: u32,
    ) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetLastError() -> u32;
}

pub(super) struct WindowsDesktopBackend {
    geometry: ScreenGeometry,
}

impl WindowsDesktopBackend {
    pub(super) fn new() -> std::result::Result<Self, DesktopError> {
        // This worker is created before it owns any windows. A zero return may also mean
        // another component already established DPI awareness, so geometry remains the
        // authoritative validation below.
        unsafe {
            SetProcessDPIAware();
        }
        ensure_interactive_desktop()?;
        Ok(Self {
            geometry: primary_geometry()?,
        })
    }

    fn ensure_interactive_desktop(&self) -> std::result::Result<(), DesktopError> {
        ensure_interactive_desktop()
    }

    fn capture_rgba(&self) -> std::result::Result<Vec<u8>, DesktopError> {
        let width = i32::try_from(self.geometry.pixel_width)
            .map_err(|_| DesktopError::capture("Windows primary display width is too large"))?;
        let height = i32::try_from(self.geometry.pixel_height)
            .map_err(|_| DesktopError::capture("Windows primary display height is too large"))?;
        let byte_count = usize::try_from(self.geometry.pixel_width)
            .ok()
            .and_then(|width| {
                usize::try_from(self.geometry.pixel_height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| DesktopError::capture("Windows screenshot buffer size overflowed"))?;
        let size_image = u32::try_from(byte_count).map_err(|_| {
            DesktopError::capture("Windows screenshot buffer exceeds the GDI DIB size limit")
        })?;

        let screen_dc = ScreenDc::acquire()?;
        let memory_dc = MemoryDc::create(screen_dc.0)?;
        let bitmap = Bitmap::create(screen_dc.0, width, height)?;
        let mut selected = SelectedObject::select(memory_dc.0, bitmap.0)?;
        let copied = unsafe {
            BitBlt(
                memory_dc.0,
                0,
                0,
                width,
                height,
                screen_dc.0,
                0,
                0,
                SRCCOPY | CAPTUREBLT,
            )
        };
        let copy_error = (copied == 0).then(last_error_code);
        selected.restore()?;
        if let Some(error) = copy_error {
            return Err(DesktopError::capture(format!(
                "Windows BitBlt could not capture the primary display (Win32 error {error})"
            )));
        }

        let mut pixels = vec![0_u8; byte_count];
        let mut info = BitmapInfo {
            header: BitmapInfoHeader {
                size: size_of::<BitmapInfoHeader>() as u32,
                width,
                height: -height,
                planes: 1,
                bit_count: 32,
                compression: BI_RGB,
                size_image,
                x_pixels_per_meter: 0,
                y_pixels_per_meter: 0,
                colors_used: 0,
                colors_important: 0,
            },
            colors: [RgbQuad {
                blue: 0,
                green: 0,
                red: 0,
                reserved: 0,
            }],
        };
        let copied_lines = unsafe {
            GetDIBits(
                memory_dc.0,
                bitmap.0,
                0,
                self.geometry.pixel_height,
                pixels.as_mut_ptr().cast(),
                &mut info,
                DIB_RGB_COLORS,
            )
        };
        if copied_lines != height {
            return Err(DesktopError::capture(format!(
                "Windows GetDIBits returned {copied_lines} of {height} primary-display scan lines (Win32 error {})",
                last_error_code()
            )));
        }
        if pixels
            .chunks_exact(4)
            .all(|pixel| pixel[0] == 0 && pixel[1] == 0 && pixel[2] == 0)
        {
            return Err(DesktopError::interactive_desktop(
                "Windows returned a completely black primary-display frame; the interactive desktop may be locked, disconnected, protected, or unavailable",
            ));
        }
        bgra_to_rgba(&mut pixels);
        Ok(pixels)
    }

    fn send_inputs(&self, inputs: &[Input]) -> std::result::Result<(), DesktopError> {
        self.ensure_interactive_desktop()?;
        let (inserted, error) = send_input_count(inputs);
        if inserted == inputs.len() as u32 {
            Ok(())
        } else {
            Err(input_insertion_error(inputs.len(), inserted, error))
        }
    }

    fn post_key(&self, key: &str, down: bool) -> std::result::Result<(), DesktopError> {
        let mapping = key_mapping(key).ok_or_else(|| {
            DesktopError::input(format!("Windows has no physical key mapping for {key:?}"))
        })?;
        self.send_inputs(&[keyboard_input(mapping, down)])
    }
}

impl DesktopBackend for WindowsDesktopBackend {
    fn geometry(&self) -> std::result::Result<ScreenGeometry, DesktopError> {
        Ok(self.geometry)
    }

    fn preflight_capture(&self) -> std::result::Result<(), DesktopError> {
        self.ensure_interactive_desktop()
    }

    fn preflight_input(&self) -> std::result::Result<(), DesktopError> {
        self.ensure_interactive_desktop()
    }

    fn capture_full(&mut self, destination: &Path) -> std::result::Result<(), DesktopError> {
        self.ensure_interactive_desktop()?;
        let pixels = match self.capture_rgba() {
            Ok(pixels) => pixels,
            Err(error) => {
                self.ensure_interactive_desktop()?;
                return Err(error);
            }
        };
        self.ensure_interactive_desktop()?;
        let image = RgbaImage::from_raw(
            self.geometry.pixel_width,
            self.geometry.pixel_height,
            pixels,
        )
        .ok_or_else(|| DesktopError::capture("Windows returned an invalid RGBA screenshot"))?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(destination)
            .map_err(|error| {
                DesktopError::capture(format!("cannot create Windows full screenshot: {error}"))
            })?;
        DynamicImage::ImageRgba8(image)
            .write_to(&mut file, ImageFormat::Png)
            .map_err(|error| {
                DesktopError::capture(format!("cannot encode Windows full screenshot: {error}"))
            })?;
        file.sync_all().map_err(|error| {
            DesktopError::capture(format!("cannot flush Windows full screenshot: {error}"))
        })
    }

    fn delay(&mut self, duration: Duration) -> std::result::Result<(), DesktopError> {
        std::thread::sleep(duration);
        Ok(())
    }

    fn mouse_move(
        &mut self,
        x: u32,
        y: u32,
        _held: &[MouseButton],
    ) -> std::result::Result<(), DesktopError> {
        let x = absolute_coordinate(x, self.geometry.pixel_width);
        let y = absolute_coordinate(y, self.geometry.pixel_height);
        self.send_inputs(&[mouse_input(
            x,
            y,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
        )])
    }

    fn mouse_down(&mut self, button: MouseButton) -> std::result::Result<(), DesktopError> {
        self.send_inputs(&[mouse_input(0, 0, 0, mouse_button_flag(button, true))])
    }

    fn mouse_up(&mut self, button: MouseButton) -> std::result::Result<(), DesktopError> {
        self.send_inputs(&[mouse_input(0, 0, 0, mouse_button_flag(button, false))])
    }

    fn mouse_wheel(&mut self, delta_x: i32, delta_y: i32) -> std::result::Result<(), DesktopError> {
        let mut inputs = Vec::with_capacity(2);
        if delta_y != 0 {
            inputs.push(mouse_input(
                0,
                0,
                delta_y.saturating_mul(WHEEL_DELTA),
                MOUSEEVENTF_WHEEL,
            ));
        }
        if delta_x != 0 {
            inputs.push(mouse_input(
                0,
                0,
                delta_x.saturating_mul(WHEEL_DELTA),
                MOUSEEVENTF_HWHEEL,
            ));
        }
        self.send_inputs(&inputs)
    }

    fn key_down(&mut self, key: &str) -> std::result::Result<(), DesktopError> {
        self.post_key(key, true)
    }

    fn key_up(&mut self, key: &str) -> std::result::Result<(), DesktopError> {
        self.post_key(key, false)
    }

    fn text_input(&mut self, text: &str) -> std::result::Result<(), DesktopError> {
        self.ensure_interactive_desktop()?;
        for unit in text.encode_utf16() {
            let inputs = [unicode_input(unit, true), unicode_input(unit, false)];
            let (inserted, error) = send_input_count(&inputs);
            if inserted != inputs.len() as u32 {
                if inserted == 1 {
                    let _ = send_input_count(&[unicode_input(unit, false)]);
                }
                return Err(input_insertion_error(inputs.len(), inserted, error));
            }
        }
        Ok(())
    }
}

fn primary_geometry() -> std::result::Result<ScreenGeometry, DesktopError> {
    let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    if width <= 0 || height <= 0 {
        return Err(DesktopError::interactive_desktop(format!(
            "Windows returned invalid primary-display geometry {width}x{height}; no capturable interactive output is available"
        )));
    }
    let pixel_width = u32::try_from(width)
        .map_err(|_| DesktopError::capture("Windows primary display width is invalid"))?;
    let pixel_height = u32::try_from(height)
        .map_err(|_| DesktopError::capture("Windows primary display height is invalid"))?;
    Ok(ScreenGeometry {
        pixel_width,
        pixel_height,
        logical_x: 0.0,
        logical_y: 0.0,
        logical_width: f64::from(pixel_width),
        logical_height: f64::from(pixel_height),
    })
}

fn ensure_interactive_desktop() -> std::result::Result<(), DesktopError> {
    let window_station = unsafe { GetProcessWindowStation() };
    if window_station == 0 {
        return Err(DesktopError::interactive_desktop(last_error_message(
            "Windows could not read the process Window Station",
        )));
    }
    let station_name = user_object_name(window_station).map_err(|message| {
        DesktopError::interactive_desktop(format!(
            "Windows could not identify the process Window Station: {message}"
        ))
    })?;
    if !station_name.eq_ignore_ascii_case("WinSta0") {
        return Err(DesktopError::interactive_desktop(format!(
            "Desktop.Play requires the interactive WinSta0 Window Station, but this process is attached to {station_name:?}"
        )));
    }

    let desktop = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS | DESKTOP_SWITCHDESKTOP) };
    if desktop == 0 {
        return Err(DesktopError::interactive_desktop(last_error_message(
            "Windows could not open the active input Desktop",
        )));
    }
    let desktop = DesktopHandle(desktop);
    let desktop_name = user_object_name(desktop.0).map_err(|message| {
        DesktopError::interactive_desktop(format!(
            "Windows could not identify the active input Desktop: {message}"
        ))
    })?;
    if !desktop_name.eq_ignore_ascii_case("Default") {
        return Err(DesktopError::interactive_desktop(format!(
            "Desktop.Play requires the normal Default input Desktop, but Windows is currently using {desktop_name:?}"
        )));
    }
    Ok(())
}

fn user_object_name(object: Handle) -> std::result::Result<String, String> {
    let mut needed = 0_u32;
    unsafe {
        GetUserObjectInformationW(object, UOI_NAME, ptr::null_mut(), 0, &mut needed);
    }
    if needed == 0 {
        return Err(last_error_message(
            "GetUserObjectInformationW did not report a name buffer",
        ));
    }
    let units = usize::try_from(needed)
        .ok()
        .and_then(|bytes| bytes.checked_add(1))
        .map(|bytes| bytes / 2)
        .filter(|units| *units <= 32_768)
        .ok_or_else(|| "Windows reported an invalid user-object name length".to_owned())?;
    let mut buffer = vec![0_u16; units];
    let succeeded = unsafe {
        GetUserObjectInformationW(
            object,
            UOI_NAME,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    };
    if succeeded == 0 {
        return Err(last_error_message("GetUserObjectInformationW failed"));
    }
    let end = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16(&buffer[..end])
        .map_err(|_| "Windows returned a malformed UTF-16 user-object name".to_owned())
}

fn absolute_coordinate(pixel: u32, extent: u32) -> i32 {
    if extent <= 1 {
        return 0;
    }
    ((u64::from(pixel) * 65_535) / u64::from(extent - 1)) as i32
}

fn mouse_button_flag(button: MouseButton, down: bool) -> u32 {
    match (button, down) {
        (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
        (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
        (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
        (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
        (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
        (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
    }
}

fn mouse_input(dx: i32, dy: i32, data: i32, flags: u32) -> Input {
    Input {
        input_type: INPUT_MOUSE,
        data: InputData {
            mouse: MouseInput {
                dx,
                dy,
                mouse_data: data as u32,
                flags,
                time: 0,
                extra_info: 0,
            },
        },
    }
}

fn keyboard_input(mapping: KeyMapping, down: bool) -> Input {
    let mut flags = if down { 0 } else { KEYEVENTF_KEYUP };
    if mapping.extended {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    Input {
        input_type: INPUT_KEYBOARD,
        data: InputData {
            keyboard: KeyboardInput {
                virtual_key: mapping.virtual_key,
                scan_code: 0,
                flags,
                time: 0,
                extra_info: 0,
            },
        },
    }
}

fn unicode_input(unit: u16, down: bool) -> Input {
    Input {
        input_type: INPUT_KEYBOARD,
        data: InputData {
            keyboard: KeyboardInput {
                virtual_key: 0,
                scan_code: unit,
                flags: KEYEVENTF_UNICODE | if down { 0 } else { KEYEVENTF_KEYUP },
                time: 0,
                extra_info: 0,
            },
        },
    }
}

fn send_input_count(inputs: &[Input]) -> (u32, u32) {
    let count = u32::try_from(inputs.len()).unwrap_or(u32::MAX);
    let inserted = unsafe {
        SendInput(
            count,
            inputs.as_ptr(),
            i32::try_from(size_of::<Input>()).expect("Win32 INPUT size fits i32"),
        )
    };
    (inserted, last_error_code())
}

fn input_insertion_error(requested: usize, inserted: u32, error: u32) -> DesktopError {
    DesktopError::input(format!(
        "Windows SendInput inserted {inserted} of {requested} requested events (Win32 error {error}); the active Desktop may have changed, or UIPI, security software, system policy, or a higher-integrity target may be blocking synthetic input"
    ))
}

fn key_mapping(key: &str) -> Option<KeyMapping> {
    if key.len() == 1 {
        let byte = key.as_bytes()[0];
        if byte.is_ascii_lowercase() {
            return Some(KeyMapping {
                virtual_key: u16::from(byte.to_ascii_uppercase()),
                extended: false,
            });
        }
        if byte.is_ascii_digit() {
            return Some(KeyMapping {
                virtual_key: u16::from(byte),
                extended: false,
            });
        }
    }
    if let Some(number) = key
        .strip_prefix('f')
        .and_then(|number| number.parse::<u16>().ok())
        .filter(|number| (1..=20).contains(number))
    {
        return Some(KeyMapping {
            virtual_key: VK_F1 + number - 1,
            extended: false,
        });
    }
    let (virtual_key, extended) = match key {
        "return" => (VK_RETURN, false),
        "escape" => (VK_ESCAPE, false),
        "space" => (VK_SPACE, false),
        "tab" => (VK_TAB, false),
        "backspace" => (VK_BACK, false),
        "delete" => (VK_DELETE, true),
        "left" => (VK_LEFT, true),
        "right" => (VK_RIGHT, true),
        "up" => (VK_UP, true),
        "down" => (VK_DOWN, true),
        "home" => (VK_HOME, true),
        "end" => (VK_END, true),
        "page_up" => (VK_PRIOR, true),
        "page_down" => (VK_NEXT, true),
        "shift" => (VK_LSHIFT, false),
        "right_shift" => (VK_RSHIFT, false),
        "control" => (VK_LCONTROL, false),
        "right_control" => (VK_RCONTROL, true),
        "option" => (VK_LMENU, false),
        "right_option" => (VK_RMENU, true),
        "command" => (VK_LWIN, true),
        "right_command" => (VK_RWIN, true),
        "caps_lock" => (VK_CAPITAL, false),
        "minus" => (VK_OEM_MINUS, false),
        "equal" => (VK_OEM_PLUS, false),
        "left_bracket" => (VK_OEM_4, false),
        "right_bracket" => (VK_OEM_6, false),
        "backslash" => (VK_OEM_5, false),
        "semicolon" => (VK_OEM_1, false),
        "quote" => (VK_OEM_7, false),
        "comma" => (VK_OEM_COMMA, false),
        "period" => (VK_OEM_PERIOD, false),
        "slash" => (VK_OEM_2, false),
        "grave" => (VK_OEM_3, false),
        _ => return None,
    };
    Some(KeyMapping {
        virtual_key,
        extended,
    })
}

fn bgra_to_rgba(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        pixel[3] = 255;
    }
}

fn last_error_code() -> u32 {
    unsafe { GetLastError() }
}

fn last_error_message(context: &str) -> String {
    format!("{context} (Win32 error {})", last_error_code())
}

struct ScreenDc(Hdc);

impl ScreenDc {
    fn acquire() -> std::result::Result<Self, DesktopError> {
        let dc = unsafe { GetDC(0) };
        if dc == 0 {
            Err(DesktopError::capture(last_error_message(
                "Windows GetDC could not access the primary display",
            )))
        } else {
            Ok(Self(dc))
        }
    }
}

impl Drop for ScreenDc {
    fn drop(&mut self) {
        unsafe {
            ReleaseDC(0, self.0);
        }
    }
}

struct MemoryDc(Hdc);

impl MemoryDc {
    fn create(compatible_with: Hdc) -> std::result::Result<Self, DesktopError> {
        let dc = unsafe { CreateCompatibleDC(compatible_with) };
        if dc == 0 {
            Err(DesktopError::capture(last_error_message(
                "Windows CreateCompatibleDC failed",
            )))
        } else {
            Ok(Self(dc))
        }
    }
}

impl Drop for MemoryDc {
    fn drop(&mut self) {
        unsafe {
            DeleteDC(self.0);
        }
    }
}

struct Bitmap(HBitmap);

impl Bitmap {
    fn create(dc: Hdc, width: i32, height: i32) -> std::result::Result<Self, DesktopError> {
        let bitmap = unsafe { CreateCompatibleBitmap(dc, width, height) };
        if bitmap == 0 {
            Err(DesktopError::capture(last_error_message(
                "Windows CreateCompatibleBitmap failed",
            )))
        } else {
            Ok(Self(bitmap))
        }
    }
}

impl Drop for Bitmap {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.0);
        }
    }
}

struct SelectedObject {
    dc: Hdc,
    previous: HGdiObject,
    active: bool,
}

impl SelectedObject {
    fn select(dc: Hdc, object: HGdiObject) -> std::result::Result<Self, DesktopError> {
        let previous = unsafe { SelectObject(dc, object) };
        if previous == 0 || previous == HGDI_ERROR {
            Err(DesktopError::capture(last_error_message(
                "Windows SelectObject could not select the capture bitmap",
            )))
        } else {
            Ok(Self {
                dc,
                previous,
                active: true,
            })
        }
    }

    fn restore(&mut self) -> std::result::Result<(), DesktopError> {
        if !self.active {
            return Ok(());
        }
        let result = unsafe { SelectObject(self.dc, self.previous) };
        if result == 0 || result == HGDI_ERROR {
            return Err(DesktopError::capture(last_error_message(
                "Windows could not remove the capture bitmap from its memory DC",
            )));
        }
        self.active = false;
        Ok(())
    }
}

impl Drop for SelectedObject {
    fn drop(&mut self) {
        if self.active {
            unsafe {
                SelectObject(self.dc, self.previous);
            }
        }
    }
}

struct DesktopHandle(HDesktop);

impl Drop for DesktopHandle {
    fn drop(&mut self) {
        unsafe {
            CloseDesktop(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win32_ffi_layouts_match_the_platform_abi() {
        assert_eq!(size_of::<BitmapInfoHeader>(), 40);
        assert_eq!(size_of::<RgbQuad>(), 4);
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(size_of::<MouseInput>(), 32);
            assert_eq!(size_of::<KeyboardInput>(), 24);
            assert_eq!(size_of::<Input>(), 40);
        }
        #[cfg(target_pointer_width = "32")]
        {
            assert_eq!(size_of::<MouseInput>(), 24);
            assert_eq!(size_of::<KeyboardInput>(), 16);
            assert_eq!(size_of::<Input>(), 28);
        }
    }

    #[test]
    fn absolute_primary_screen_coordinates_cover_the_win32_range() {
        assert_eq!(absolute_coordinate(0, 1920), 0);
        assert_eq!(absolute_coordinate(1919, 1920), 65_535);
        assert_eq!(absolute_coordinate(0, 1), 0);
    }

    #[test]
    fn every_windows_supported_normalized_key_has_a_mapping() {
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
            assert!(key_mapping(key).is_some(), "missing mapping for {key}");
        }
        for byte in b'a'..=b'z' {
            assert!(key_mapping(&char::from(byte).to_string()).is_some());
        }
        for byte in b'0'..=b'9' {
            assert!(key_mapping(&char::from(byte).to_string()).is_some());
        }
        for number in 1..=20 {
            assert!(key_mapping(&format!("f{number}")).is_some());
        }
        assert!(key_mapping("function").is_none());
    }

    #[test]
    fn bgra_conversion_makes_png_pixels_opaque_rgba() {
        let mut pixels = vec![3, 2, 1, 0, 30, 20, 10, 44];
        bgra_to_rgba(&mut pixels);
        assert_eq!(pixels, vec![1, 2, 3, 255, 10, 20, 30, 255]);
    }
}
