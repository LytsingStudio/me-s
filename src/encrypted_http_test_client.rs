// Shared by several test binaries, each exercising a different subset of the client.
#![allow(dead_code)]

use std::{
    collections::HashMap,
    io::{self, Read},
    sync::{Arc, Mutex},
};

/// Keeps existing route assertions focused on inner HTTP semantics; outer-wire tests use raw reqwest.
pub(crate) struct Client {
    network: reqwest::blocking::Client,
    channels: Mutex<HashMap<String, Arc<me_transport::blocking::Transport>>>,
}
impl Client {
    pub(crate) fn new() -> Self {
        Self {
            network: reqwest::blocking::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap(),
            channels: Mutex::new(HashMap::new()),
        }
    }
    pub(crate) fn get(&self, url: impl AsRef<str>) -> RequestBuilder<'_> {
        RequestBuilder {
            owner: self,
            builder: self.network.get(url.as_ref()),
        }
    }
    pub(crate) fn post(&self, url: impl AsRef<str>) -> RequestBuilder<'_> {
        RequestBuilder {
            owner: self,
            builder: self.network.post(url.as_ref()),
        }
    }
}

pub(crate) struct RequestBuilder<'a> {
    owner: &'a Client,
    builder: reqwest::blocking::RequestBuilder,
}
impl RequestBuilder<'_> {
    pub(crate) fn header(mut self, name: impl AsRef<str>, value: impl AsRef<str>) -> Self {
        self.builder = self.builder.header(name.as_ref(), value.as_ref());
        self
    }
    pub(crate) fn json(mut self, value: &impl serde::Serialize) -> Self {
        self.builder = self.builder.json(value);
        self
    }
    pub(crate) fn send(self) -> io::Result<Response> {
        let request = self.builder.build().map_err(io::Error::other)?;
        let url = request.url();
        if !url.path().starts_with("/api/") {
            let response = self
                .owner
                .network
                .execute(request)
                .map_err(io::Error::other)?;
            return Ok(Response {
                status: response.status(),
                headers: response.headers().clone(),
                body: Box::new(response),
            });
        }
        let endpoint = url.origin().ascii_serialization();
        let transport = Arc::clone(
            self.owner
                .channels
                .lock()
                .unwrap()
                .entry(endpoint.clone())
                .or_insert_with(|| Arc::new(me_transport::blocking::Transport::new(endpoint))),
        );
        let body = request
            .body()
            .map(|b| b.as_bytes().unwrap())
            .unwrap_or_default();
        let head = me_transport::RequestHead {
            method: request.method().as_str().into(),
            url: format!(
                "{}{}",
                url.path(),
                url.query().map(|q| format!("?{q}")).unwrap_or_default()
            ),
            headers: request
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap().to_owned()))
                .collect(),
            body_length: body.len(),
        };
        let response = transport.request(&self.owner.network, &head, body)?;
        let mut headers = reqwest::header::HeaderMap::new();
        for (key, value) in response.head.headers {
            headers.append(
                reqwest::header::HeaderName::from_bytes(key.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        Ok(Response {
            status: reqwest::StatusCode::from_u16(response.head.status).unwrap(),
            headers,
            body: Box::new(response.body),
        })
    }
}

pub(crate) struct Response {
    status: reqwest::StatusCode,
    headers: reqwest::header::HeaderMap,
    body: Box<dyn Read + Send>,
}
impl Response {
    pub(crate) fn status(&self) -> reqwest::StatusCode {
        self.status
    }
    pub(crate) fn headers(&self) -> &reqwest::header::HeaderMap {
        &self.headers
    }
    pub(crate) fn error_for_status(self) -> io::Result<Self> {
        if self.status.is_client_error() || self.status.is_server_error() {
            Err(io::Error::other(self.status.to_string()))
        } else {
            Ok(self)
        }
    }
    pub(crate) fn bytes(mut self) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.body.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    pub(crate) fn json<T: serde::de::DeserializeOwned>(self) -> io::Result<T> {
        Ok(serde_json::from_slice(&self.bytes()?)?)
    }
    pub(crate) fn text(self) -> io::Result<String> {
        String::from_utf8(self.bytes()?).map_err(io::Error::other)
    }
}
impl Read for Response {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.body.read(bytes)
    }
}

pub(crate) fn get(url: impl AsRef<str>) -> io::Result<Response> {
    Client::new().get(url).send()
}
