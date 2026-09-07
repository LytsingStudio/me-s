use super::*;
use std::cell::RefCell;

struct WebState {
    handshake: Option<HandshakeState>,
    channel: Option<Arc<Channel>>,
    input: Vec<u8>,
    output: Vec<u8>,
}
impl Default for WebState {
    fn default() -> Self {
        Self {
            handshake: None,
            channel: None,
            input: vec![0; MAX_RECORD_BYTES],
            output: Vec::new(),
        }
    }
}
thread_local! { static STATE: RefCell<WebState> = RefCell::new(WebState::default()); }

#[unsafe(no_mangle)]
pub extern "C" fn me_input() -> *mut u8 {
    STATE.with(|state| state.borrow_mut().input.as_mut_ptr())
}
#[unsafe(no_mangle)]
pub extern "C" fn me_output() -> *const u8 {
    STATE.with(|state| state.borrow().output.as_ptr())
}
#[unsafe(no_mangle)]
pub extern "C" fn me_start() -> i32 {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.channel = None;
        state.handshake = None;
        match initiate() {
            Ok((handshake, message)) => {
                state.handshake = Some(handshake);
                state.output = message;
                state.output.len() as i32
            }
            Err(_) => -1,
        }
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn me_finish(length: usize) -> i32 {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if length != 48 {
            return -1;
        }
        let Some(handshake) = state.handshake.take() else {
            return -1;
        };
        match finish(handshake, &state.input[..length]) {
            Ok(channel) => {
                state.channel = Some(channel);
                0
            }
            Err(_) => -1,
        }
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn me_next_request() -> i64 {
    STATE.with(|state| {
        state
            .borrow()
            .channel
            .as_ref()
            .and_then(|channel| channel.next_request().ok())
            .map(i64::from)
            .unwrap_or(-1)
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn me_seal(request: u32, block: u32, kind: u32, length: usize) -> i32 {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if length > CHUNK_BYTES || kind > u32::from(END) {
            return -1;
        }
        let Some(channel) = state.channel.as_ref() else {
            return -1;
        };
        match channel.seal(request, block, kind as u8, &state.input[..length]) {
            Ok(output) => {
                state.output = output;
                state.output.len() as i32
            }
            Err(_) => -1,
        }
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn me_open(request: u32, block: u32, length: usize) -> i32 {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        if length > MAX_RECORD_BYTES {
            return -1;
        }
        let Some(channel) = state.channel.as_ref() else {
            return -1;
        };
        match channel.open(request, block, &state.input[..length]) {
            Ok(output) => {
                state.output = output;
                state.output.len() as i32
            }
            Err(_) => -1,
        }
    })
}
