use std::{
    collections::BTreeSet,
    io::{self, Cursor, Read},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use snow::{
    Builder, HandshakeState, StatelessTransportState,
    params::{CipherChoice, DHChoice, HashChoice},
    resolvers::{CryptoResolver, DefaultResolver},
    types::{Cipher, Dh, Hash, Random},
};

#[cfg(test)]
mod tests;

#[cfg(target_arch = "wasm32")]
mod wasm;

#[cfg(all(not(target_arch = "wasm32"), feature = "blocking"))]
pub mod blocking;

#[cfg(all(not(target_arch = "wasm32"), feature = "async"))]
pub mod async_http;

pub const HANDSHAKE_PATH: &str = "/_me/handshake";
pub const REQUEST_PATH: &str = "/_me/request";
pub const CONTENT_TYPE: &str = "application/octet-stream";
pub const PROTOCOL: &str = "Noise_NN_25519_ChaChaPoly_SHA256";
pub const PROLOGUE: &[u8] = b"ME encrypted HTTP v1";
pub const CHUNK_BYTES: usize = 32 * 1024;
pub const MAX_RECORD_BYTES: usize = CHUNK_BYTES + 17;
pub const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
pub const SESSION_ID_BYTES: usize = 16;
pub const HEADER: u8 = 0;
pub const DATA: u8 = 1;
pub const END: u8 = 2;

pub fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid encrypted transport message",
    )
}

pub fn random_fill(bytes: &mut [u8]) -> io::Result<()> {
    #[cfg(target_arch = "wasm32")]
    {
        #[link(wasm_import_module = "env")]
        unsafe extern "C" {
            fn me_random_fill(ptr: *mut u8, len: usize) -> u32;
        }
        if unsafe { me_random_fill(bytes.as_mut_ptr(), bytes.len()) } != 0 {
            return Err(io::Error::other("secure random source unavailable"));
        }
        Ok(())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        getrandom::fill(bytes).map_err(|_| io::Error::other("secure random source unavailable"))
    }
}

struct PlatformResolver;
impl Random for PlatformResolver {
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), snow::Error> {
        random_fill(dest).map_err(|_| snow::Error::Rng)
    }
}
impl CryptoResolver for PlatformResolver {
    fn resolve_rng(&self) -> Option<Box<dyn Random>> {
        Some(Box::new(Self))
    }
    fn resolve_dh(&self, choice: &DHChoice) -> Option<Box<dyn Dh>> {
        DefaultResolver.resolve_dh(choice)
    }
    fn resolve_cipher(&self, choice: &CipherChoice) -> Option<Box<dyn Cipher>> {
        DefaultResolver.resolve_cipher(choice)
    }
    fn resolve_hash(&self, choice: &HashChoice) -> Option<Box<dyn Hash>> {
        DefaultResolver.resolve_hash(choice)
    }
}

fn handshake(initiator: bool) -> io::Result<HandshakeState> {
    let builder = Builder::with_resolver(
        PROTOCOL.parse().map_err(|_| invalid())?,
        Box::new(PlatformResolver),
    )
    .prologue(PROLOGUE)
    .map_err(|_| invalid())?;
    if initiator {
        builder.build_initiator()
    } else {
        builder.build_responder()
    }
    .map_err(|_| invalid())
}

pub fn initiate() -> io::Result<(HandshakeState, Vec<u8>)> {
    let mut state = handshake(true)?;
    // Snow reserves tag space even for NN's unencrypted first handshake message.
    let mut message = vec![0; 48];
    let size = state
        .write_message(&[], &mut message)
        .map_err(|_| invalid())?;
    message.truncate(size);
    Ok((state, message))
}

pub fn respond(message: &[u8]) -> io::Result<(Arc<Channel>, Vec<u8>)> {
    if message.len() != 32 {
        return Err(invalid());
    }
    let mut state = handshake(false)?;
    let mut payload = [0; 64];
    if state
        .read_message(message, &mut payload)
        .map_err(|_| invalid())?
        != 0
    {
        return Err(invalid());
    }
    let mut reply = vec![0; 48];
    let size = state
        .write_message(&[], &mut reply)
        .map_err(|_| invalid())?;
    reply.truncate(size);
    Ok((Channel::from_handshake(state)?, reply))
}

pub fn finish(mut state: HandshakeState, message: &[u8]) -> io::Result<Arc<Channel>> {
    if message.len() != 48 {
        return Err(invalid());
    }
    let mut payload = [0; 64];
    if state
        .read_message(message, &mut payload)
        .map_err(|_| invalid())?
        != 0
    {
        return Err(invalid());
    }
    Channel::from_handshake(state)
}

pub struct Channel {
    noise: StatelessTransportState,
    next_request: AtomicU32,
    received: Mutex<ReplayWindow>,
}

#[derive(Default)]
struct ReplayWindow {
    highest: u32,
    seen: BTreeSet<u32>,
}

impl Channel {
    fn from_handshake(state: HandshakeState) -> io::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            noise: state
                .into_stateless_transport_mode()
                .map_err(|_| invalid())?,
            next_request: AtomicU32::new(0),
            received: Mutex::new(ReplayWindow::default()),
        }))
    }

    pub fn next_request(&self) -> io::Result<u32> {
        self.next_request
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| io::Error::other("encrypted transport key exhausted"))
    }

    // A request number is consumed only after every record has authenticated, before dispatch.
    pub fn accept_request(&self, number: u32) -> io::Result<()> {
        let mut window = self.received.lock().map_err(|_| invalid())?;
        if number == u32::MAX
            || number < window.highest.saturating_sub(1023)
            || !window.seen.insert(number)
        {
            return Err(invalid());
        }
        window.highest = window.highest.max(number);
        let minimum = window.highest.saturating_sub(1023);
        window.seen = window.seen.split_off(&minimum);
        Ok(())
    }

    pub fn seal(&self, request: u32, block: u32, kind: u8, bytes: &[u8]) -> io::Result<Vec<u8>> {
        if request == u32::MAX
            || block == u32::MAX
            || bytes.len() > CHUNK_BYTES
            || kind > END
            || (kind == END && !bytes.is_empty())
        {
            return Err(invalid());
        }
        let mut payload = Vec::with_capacity(bytes.len() + 1);
        payload.push(kind);
        payload.extend_from_slice(bytes);
        let mut output = vec![0; payload.len() + 18];
        // Noise splits independent keys for each direction. Each request owns a disjoint nonce range.
        let nonce = (u64::from(request) << 32) | u64::from(block);
        let size = self
            .noise
            .write_message(nonce, &payload, &mut output[2..])
            .map_err(|_| invalid())?;
        output[..2].copy_from_slice(&(size as u16).to_be_bytes());
        Ok(output)
    }

    pub fn open(&self, request: u32, block: u32, ciphertext: &[u8]) -> io::Result<Vec<u8>> {
        if request == u32::MAX
            || block == u32::MAX
            || !(17..=MAX_RECORD_BYTES).contains(&ciphertext.len())
        {
            return Err(invalid());
        }
        let mut output = vec![0; ciphertext.len() - 16];
        let nonce = (u64::from(request) << 32) | u64::from(block);
        let size = self
            .noise
            .read_message(nonce, ciphertext, &mut output)
            .map_err(|_| invalid())?;
        output.truncate(size);
        if output.is_empty() || output[0] > END || (output[0] == END && output.len() != 1) {
            return Err(invalid());
        }
        Ok(output)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestHead {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body_length: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body_length: Option<u64>,
}

pub struct EncryptReader<R> {
    reader: R,
    channel: Arc<Channel>,
    request: u32,
    block: u32,
    pending: Cursor<Vec<u8>>,
    ended: bool,
}

impl<R: Read> EncryptReader<R> {
    pub fn new(channel: Arc<Channel>, request: u32, head: &[u8], reader: R) -> io::Result<Self> {
        let pending = Cursor::new(channel.seal(request, 0, HEADER, head)?);
        Ok(Self {
            reader,
            channel,
            request,
            block: 1,
            pending,
            ended: false,
        })
    }
}

impl<R: Read> Read for EncryptReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let size = self.pending.read(out)?;
        if size != 0 || self.ended {
            return Ok(size);
        }
        let mut bytes = [0; CHUNK_BYTES];
        let size = self.reader.read(&mut bytes)?;
        let kind = if size == 0 { END } else { DATA };
        self.pending =
            Cursor::new(
                self.channel
                    .seal(self.request, self.block, kind, &bytes[..size])?,
            );
        self.block = self.block.checked_add(1).ok_or_else(invalid)?;
        self.ended = size == 0;
        self.pending.read(out)
    }
}

pub fn read_record(
    reader: &mut impl Read,
    channel: &Channel,
    request: u32,
    block: u32,
) -> io::Result<Vec<u8>> {
    let mut length = [0; 2];
    reader.read_exact(&mut length)?;
    let length = usize::from(u16::from_be_bytes(length));
    if !(17..=MAX_RECORD_BYTES).contains(&length) {
        return Err(invalid());
    }
    let mut encrypted = vec![0; length];
    reader.read_exact(&mut encrypted)?;
    channel.open(request, block, &encrypted)
}

pub struct DecryptReader<R> {
    reader: R,
    channel: Arc<Channel>,
    request: u32,
    block: u32,
    pending: Cursor<Vec<u8>>,
    ended: bool,
    expected_length: Option<u64>,
    received_length: u64,
}

impl<R: Read> DecryptReader<R> {
    pub fn new(mut reader: R, channel: Arc<Channel>, request: u32) -> io::Result<(Vec<u8>, Self)> {
        let head = read_record(&mut reader, &channel, request, 0)?;
        if head.first() != Some(&HEADER) {
            return Err(invalid());
        }
        Ok((
            head[1..].to_vec(),
            Self {
                reader,
                channel,
                request,
                block: 1,
                pending: Cursor::new(Vec::new()),
                ended: false,
                expected_length: None,
                received_length: 0,
            },
        ))
    }

    pub fn set_expected_length(&mut self, length: Option<u64>) {
        self.expected_length = length;
    }
}

impl<R: Read> Read for DecryptReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let size = self.pending.read(out)?;
        if size != 0 || self.ended {
            return Ok(size);
        }
        let record = read_record(&mut self.reader, &self.channel, self.request, self.block)?;
        self.block = self.block.checked_add(1).ok_or_else(invalid)?;
        match record[0] {
            DATA if record.len() > 1 => {
                self.received_length = self
                    .received_length
                    .checked_add((record.len() - 1) as u64)
                    .ok_or_else(invalid)?;
                if self
                    .expected_length
                    .is_some_and(|expected| self.received_length > expected)
                {
                    return Err(invalid());
                }
                self.pending = Cursor::new(record[1..].to_vec());
                self.pending.read(out)
            }
            END => {
                if self
                    .expected_length
                    .is_some_and(|expected| self.received_length != expected)
                {
                    return Err(invalid());
                }
                // HTTP EOF alone is not a successful encrypted stream: an authenticated END is required.
                if self.reader.read(&mut [0])? != 0 {
                    return Err(invalid());
                }
                self.ended = true;
                Ok(0)
            }
            _ => Err(invalid()),
        }
    }
}

pub fn encode_request(
    channel: Arc<Channel>,
    number: u32,
    head: &RequestHead,
    body: &[u8],
) -> io::Result<Vec<u8>> {
    if body.len() != head.body_length || body.len() > MAX_REQUEST_BYTES {
        return Err(invalid());
    }
    let mut reader = EncryptReader::new(
        channel,
        number,
        &serde_json::to_vec(head)?,
        Cursor::new(body),
    )?;
    let mut encoded = Vec::new();
    reader.read_to_end(&mut encoded)?;
    Ok(encoded)
}

pub fn decode_request(
    channel: Arc<Channel>,
    number: u32,
    bytes: &[u8],
) -> io::Result<(RequestHead, Vec<u8>)> {
    let (head, mut reader) = DecryptReader::new(Cursor::new(bytes), Arc::clone(&channel), number)?;
    let head: RequestHead = serde_json::from_slice(&head)?;
    if head.body_length > MAX_REQUEST_BYTES {
        return Err(invalid());
    }
    reader.set_expected_length(Some(head.body_length as u64));
    let mut body = Vec::new();
    reader
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut body)?;
    if body.len() > MAX_REQUEST_BYTES {
        return Err(invalid());
    }
    channel.accept_request(number)?;
    Ok((head, body))
}
