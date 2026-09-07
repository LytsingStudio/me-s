use std::{
    collections::HashMap,
    io::{self, Cursor, Read},
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use me_transport::{Channel, EncryptReader, RequestHead, ResponseHead};
use tiny_http::{Header, Method, Response, StatusCode};

pub(crate) type HttpResponse = Response<Box<dyn Read + Send>>;
const MAX_CHANNELS: usize = 1024;
const CHANNEL_LIFETIME: Duration = Duration::from_secs(3600);
const MAX_WIRE_REQUEST: usize = me_transport::MAX_REQUEST_BYTES + 128 * 1024;

/// Only authenticated inner requests reach the business router.
pub(crate) struct Request {
    method: Method,
    url: String,
    headers: Vec<Header>,
    body: Cursor<Vec<u8>>,
}

impl Request {
    pub(crate) fn new(head: RequestHead, body: Vec<u8>) -> io::Result<Self> {
        if !matches!(
            head.method.as_str(),
            "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" | "OPTIONS"
        ) || !head.url.starts_with("/api/")
            || head.url.bytes().any(|b| b <= 32 || b == 127 || b == b'#')
            || head.headers.len() > 128
        {
            return Err(me_transport::invalid());
        }
        let headers = head
            .headers
            .into_iter()
            .map(|(name, value)| {
                if name.is_empty() || value.bytes().any(|b| b < 32 && b != b'\t' || b == 127) {
                    return Err(me_transport::invalid());
                }
                Header::from_bytes(name, value).map_err(|_| me_transport::invalid())
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            method: Method::from_str(&head.method).map_err(|_| me_transport::invalid())?,
            url: head.url,
            headers,
            body: Cursor::new(body),
        })
    }

    pub(crate) fn method(&self) -> &Method {
        &self.method
    }
    pub(crate) fn url(&self) -> &str {
        &self.url
    }
    pub(crate) fn headers(&self) -> &[Header] {
        &self.headers
    }
    pub(crate) fn body_length(&self) -> Option<usize> {
        Some(self.body.get_ref().len())
    }
    pub(crate) fn as_reader(&mut self) -> &mut dyn Read {
        &mut self.body
    }
}

struct Session {
    channel: Arc<Channel>,
    created: Instant,
}

#[derive(Default)]
pub(crate) struct EncryptedHttp {
    sessions: Mutex<HashMap<[u8; me_transport::SESSION_ID_BYTES], Session>>,
}

fn outer_response(status: u16, bytes: Vec<u8>) -> HttpResponse {
    Response::from_data(bytes)
        .with_status_code(StatusCode(status))
        .with_header(Header::from_bytes("Content-Type", me_transport::CONTENT_TYPE).unwrap())
        .with_header(Header::from_bytes("Cache-Control", "no-store").unwrap())
        .boxed()
}

fn read_outer(request: &mut tiny_http::Request, limit: usize) -> io::Result<Vec<u8>> {
    if request.body_length().is_some_and(|n| n > limit) {
        return Err(me_transport::invalid());
    }
    let mut body = Vec::new();
    request
        .as_reader()
        .take((limit + 1) as u64)
        .read_to_end(&mut body)?;
    if body.len() > limit {
        return Err(me_transport::invalid());
    }
    Ok(body)
}

impl EncryptedHttp {
    fn handshake(&self, body: &[u8]) -> io::Result<HttpResponse> {
        let mut sessions = self.sessions.lock().map_err(|_| me_transport::invalid())?;
        sessions.retain(|_, session| session.created.elapsed() < CHANNEL_LIFETIME);
        if sessions.len() >= MAX_CHANNELS {
            return Ok(outer_response(503, Vec::new()));
        }
        let (channel, reply) = me_transport::respond(body)?;
        let mut id = [0; me_transport::SESSION_ID_BYTES];
        loop {
            me_transport::random_fill(&mut id)?;
            if !sessions.contains_key(&id) {
                break;
            }
        }
        sessions.insert(
            id,
            Session {
                channel,
                created: Instant::now(),
            },
        );
        let mut bytes = id.to_vec();
        bytes.extend(reply);
        Ok(outer_response(200, bytes))
    }

    fn encrypted_request(
        &self,
        body: &[u8],
        route: impl FnOnce(&mut Request) -> HttpResponse,
    ) -> io::Result<HttpResponse> {
        if body.len() < 20 {
            return Err(me_transport::invalid());
        }
        let id: [u8; 16] = body[..16].try_into().unwrap();
        let number = u32::from_be_bytes(body[16..20].try_into().unwrap());
        let channel = {
            let mut sessions = self.sessions.lock().map_err(|_| me_transport::invalid())?;
            if sessions
                .get(&id)
                .is_some_and(|s| s.created.elapsed() >= CHANNEL_LIFETIME)
            {
                sessions.remove(&id);
            }
            sessions.get(&id).map(|s| Arc::clone(&s.channel))
        };
        // 410 is emitted only before decoding or dispatch; it is the sole safe re-handshake retry.
        let Some(channel) = channel else {
            return Ok(outer_response(410, Vec::new()));
        };
        let (head, bytes) =
            me_transport::decode_request(Arc::clone(&channel), number, &body[20..])?;
        let mut request = Request::new(head, bytes)?;
        let response = route(&mut request);
        let head = ResponseHead {
            status: response.status_code().0,
            headers: response
                .headers()
                .iter()
                .map(|h| (h.field.to_string(), h.value.to_string()))
                .collect(),
            body_length: response.data_length().map(|n| n as u64),
        };
        let stream = EncryptReader::new(
            channel,
            number,
            &serde_json::to_vec(&head)?,
            response.into_reader(),
        )?;
        Ok(Response::new(
            StatusCode(200),
            vec![
                Header::from_bytes("Content-Type", me_transport::CONTENT_TYPE).unwrap(),
                Header::from_bytes("Cache-Control", "no-store").unwrap(),
            ],
            Box::new(stream) as Box<dyn Read + Send>,
            None,
            None,
        ))
    }

    pub(crate) fn serve(
        &self,
        mut request: tiny_http::Request,
        asset: impl FnOnce(&str) -> Option<HttpResponse>,
        route: impl FnOnce(&mut Request) -> HttpResponse,
    ) {
        let response = if request.method() == &Method::Get {
            // The explicit asset router cannot access application state, credentials or request headers.
            asset(request.url()).unwrap_or_else(|| outer_response(426, Vec::new()))
        } else if request.method() == &Method::Post
            && matches!(
                request.url(),
                me_transport::HANDSHAKE_PATH | me_transport::REQUEST_PATH
            )
        {
            let handshake = request.url() == me_transport::HANDSHAKE_PATH;
            let result = read_outer(&mut request, if handshake { 32 } else { MAX_WIRE_REQUEST })
                .and_then(|body| {
                    if handshake {
                        self.handshake(&body)
                    } else {
                        self.encrypted_request(&body, route)
                    }
                });
            result.unwrap_or_else(|_| outer_response(400, Vec::new()))
        } else {
            outer_response(426, Vec::new())
        };
        let _ = request.respond(response);
    }
}

#[cfg(test)]
#[path = "encrypted_http_test_client.rs"]
pub(crate) mod test_client;

#[cfg(test)]
#[path = "encrypted_http_tests.rs"]
mod tests;
