use crate::{Channel, RequestHead, ResponseHead};
use std::{
    io,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
struct Session {
    id: [u8; 16],
    channel: Arc<Channel>,
}

pub struct Transport {
    endpoint: String,
    session: tokio::sync::Mutex<Option<Session>>,
    cookie: Mutex<String>,
}

struct Records {
    response: reqwest::Response,
    channel: Arc<Channel>,
    number: u32,
    block: u32,
    pending: Vec<u8>,
    offset: usize,
}
impl Records {
    async fn exact(&mut self, length: usize) -> io::Result<Vec<u8>> {
        let mut bytes = vec![0; length];
        let mut written = 0;
        while written < length {
            if self.offset == self.pending.len() {
                let chunk = self
                    .response
                    .chunk()
                    .await
                    .map_err(io::Error::other)?
                    .ok_or_else(crate::invalid)?;
                self.pending = chunk.to_vec();
                self.offset = 0;
                if self.pending.is_empty() {
                    continue;
                }
            }
            let n = (length - written).min(self.pending.len() - self.offset);
            bytes[written..written + n]
                .copy_from_slice(&self.pending[self.offset..self.offset + n]);
            self.offset += n;
            written += n;
        }
        Ok(bytes)
    }
    async fn next(&mut self) -> io::Result<Vec<u8>> {
        let size = self.exact(2).await?;
        let size = usize::from(u16::from_be_bytes([size[0], size[1]]));
        if !(17..=crate::MAX_RECORD_BYTES).contains(&size) {
            return Err(crate::invalid());
        }
        let encrypted = self.exact(size).await?;
        let bytes = self.channel.open(self.number, self.block, &encrypted)?;
        self.block = self.block.checked_add(1).ok_or_else(crate::invalid)?;
        Ok(bytes)
    }
    async fn end(&mut self) -> io::Result<()> {
        if self.offset != self.pending.len() {
            return Err(crate::invalid());
        }
        while let Some(bytes) = self.response.chunk().await.map_err(io::Error::other)? {
            if !bytes.is_empty() {
                return Err(crate::invalid());
            }
        }
        Ok(())
    }
}

pub struct Response {
    pub head: ResponseHead,
    records: Records,
    received: u64,
    ended: bool,
}
impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.head
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
    pub async fn chunk(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.ended {
            return Ok(None);
        }
        let record = self.records.next().await?;
        match record[0] {
            crate::DATA if record.len() > 1 => {
                self.received = self
                    .received
                    .checked_add((record.len() - 1) as u64)
                    .ok_or_else(crate::invalid)?;
                if self.head.body_length.is_some_and(|n| self.received > n) {
                    return Err(crate::invalid());
                }
                Ok(Some(record[1..].to_vec()))
            }
            crate::END => {
                if self.head.body_length.is_some_and(|n| self.received != n) {
                    return Err(crate::invalid());
                }
                self.records.end().await?;
                self.ended = true;
                Ok(None)
            }
            _ => Err(crate::invalid()),
        }
    }
    pub async fn bytes(mut self) -> io::Result<Vec<u8>> {
        let mut body = Vec::new();
        while let Some(chunk) = self.chunk().await? {
            body.extend(chunk);
        }
        Ok(body)
    }
}

impl Transport {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_owned(),
            session: tokio::sync::Mutex::new(None),
            cookie: Mutex::new(String::new()),
        }
    }
    async fn session(&self, client: &reqwest::Client) -> io::Result<Session> {
        let mut session = self.session.lock().await;
        if let Some(session) = session.as_ref() {
            return Ok(session.clone());
        }
        let (state, hello) = crate::initiate()?;
        let mut response = client
            .post(format!("{}{}", self.endpoint, crate::HANDSHAKE_PATH))
            .header("Content-Type", crate::CONTENT_TYPE)
            .body(hello)
            .send()
            .await
            .map_err(io::Error::other)?;
        if response.status() != 200
            || response
                .headers()
                .get("Content-Type")
                .and_then(|v| v.to_str().ok())
                != Some(crate::CONTENT_TYPE)
        {
            return Err(io::Error::other("无法建立安全连接，请确认服务版本与地址"));
        }
        let mut reply = Vec::new();
        while let Some(bytes) = response.chunk().await.map_err(io::Error::other)? {
            if reply.len() + bytes.len() > 64 {
                return Err(crate::invalid());
            }
            reply.extend(bytes);
        }
        if reply.len() != 64 {
            return Err(crate::invalid());
        }
        let active = Session {
            id: reply[..16].try_into().unwrap(),
            channel: crate::finish(state, &reply[16..])?,
        };
        *session = Some(active.clone());
        Ok(active)
    }
    pub async fn request(
        &self,
        client: &reqwest::Client,
        head: &RequestHead,
        body: &[u8],
    ) -> io::Result<Response> {
        let mut head = head.clone();
        head.headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case("cookie"));
        let cookie = self.cookie.lock().map_err(|_| crate::invalid())?.clone();
        if !cookie.is_empty() {
            head.headers.push(("Cookie".into(), cookie));
        }
        for attempt in 0..2 {
            let session = self.session(client).await?;
            let number = session.channel.next_request()?;
            let mut packet = session.id.to_vec();
            packet.extend(number.to_be_bytes());
            packet.extend(crate::encode_request(
                Arc::clone(&session.channel),
                number,
                &head,
                body,
            )?);
            let response = client
                .post(format!("{}{}", self.endpoint, crate::REQUEST_PATH))
                .header("Content-Type", crate::CONTENT_TYPE)
                .body(packet)
                .send()
                .await
                .map_err(io::Error::other)?;
            if response.status() == 410 && attempt == 0 {
                let mut current = self.session.lock().await;
                if current.as_ref().is_some_and(|s| s.id == session.id) {
                    *current = None;
                }
                continue;
            }
            if response.status() != 200
                || response
                    .headers()
                    .get("Content-Type")
                    .and_then(|v| v.to_str().ok())
                    != Some(crate::CONTENT_TYPE)
            {
                return Err(io::Error::other("安全连接已中断，请重试"));
            }
            let mut records = Records {
                response,
                channel: session.channel,
                number,
                block: 0,
                pending: Vec::new(),
                offset: 0,
            };
            let bytes = records.next().await?;
            if bytes[0] != crate::HEADER {
                return Err(crate::invalid());
            }
            let mut head: ResponseHead = serde_json::from_slice(&bytes[1..])?;
            if !(200..=599).contains(&head.status) {
                return Err(crate::invalid());
            }
            for (_, value) in head
                .headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
            {
                *self.cookie.lock().map_err(|_| crate::invalid())? =
                    value.split(';').next().unwrap_or_default().to_owned();
            }
            head.headers
                .retain(|(name, _)| !name.eq_ignore_ascii_case("set-cookie"));
            return Ok(Response {
                head,
                records,
                received: 0,
                ended: false,
            });
        }
        Err(crate::invalid())
    }
}
