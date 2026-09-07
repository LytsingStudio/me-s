use crate::{Channel, DecryptReader, RequestHead, ResponseHead};
use reqwest::blocking::Client;
use std::{
    io::{self, Read},
    sync::{Arc, Mutex},
};

#[derive(Clone)]
struct Session {
    id: [u8; 16],
    channel: Arc<Channel>,
}

pub struct Transport {
    endpoint: String,
    session: Mutex<Option<Session>>,
}

pub struct Response {
    pub head: ResponseHead,
    pub body: DecryptReader<reqwest::blocking::Response>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.head
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
    pub fn json<T: serde::de::DeserializeOwned>(mut self) -> io::Result<T> {
        let mut body = Vec::new();
        self.body.read_to_end(&mut body)?;
        Ok(serde_json::from_slice(&body)?)
    }
}

impl Transport {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_owned(),
            session: Mutex::new(None),
        }
    }

    fn session(&self, client: &Client) -> io::Result<Session> {
        let mut session = self.session.lock().map_err(|_| crate::invalid())?;
        if let Some(session) = session.as_ref() {
            return Ok(session.clone());
        }
        let (state, hello) = crate::initiate()?;
        let response = client
            .post(format!("{}{}", self.endpoint, crate::HANDSHAKE_PATH))
            .header("Content-Type", crate::CONTENT_TYPE)
            .body(hello)
            .send()
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
        response.take(65).read_to_end(&mut reply)?;
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

    pub fn request(
        &self,
        client: &Client,
        head: &RequestHead,
        body: &[u8],
    ) -> io::Result<Response> {
        for attempt in 0..2 {
            let session = self.session(client)?;
            let number = session.channel.next_request()?;
            let mut packet = session.id.to_vec();
            packet.extend(number.to_be_bytes());
            packet.extend(crate::encode_request(
                Arc::clone(&session.channel),
                number,
                head,
                body,
            )?);
            let response = client
                .post(format!("{}{}", self.endpoint, crate::REQUEST_PATH))
                .header("Content-Type", crate::CONTENT_TYPE)
                .body(packet)
                .send()
                .map_err(io::Error::other)?;
            if response.status() == 410 && attempt == 0 {
                // The server guarantees that an unknown channel is rejected before dispatch.
                let mut current = self.session.lock().map_err(|_| crate::invalid())?;
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
            let (head, mut body) = DecryptReader::new(response, session.channel, number)?;
            let head: ResponseHead = serde_json::from_slice(&head)?;
            if !(200..=599).contains(&head.status) {
                return Err(crate::invalid());
            }
            body.set_expected_length(head.body_length);
            return Ok(Response { head, body });
        }
        Err(crate::invalid())
    }
}
