use super::*;
use std::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    thread,
};

struct Fixture {
    endpoint: String,
    transport: Arc<EncryptedHttp>,
    stop: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Fixture {
    fn start() -> Self {
        let server = tiny_http::Server::http(("127.0.0.1", 0)).unwrap();
        let endpoint = format!("http://{}", server.server_addr());
        let transport = Arc::new(EncryptedHttp::default());
        let stop = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let worker = {
            let transport = Arc::clone(&transport);
            let stop = Arc::clone(&stop);
            let calls = Arc::clone(&calls);
            thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let Some(request) = server.recv_timeout(Duration::from_millis(20)).unwrap()
                    else {
                        continue;
                    };
                    let transport = Arc::clone(&transport);
                    let calls = Arc::clone(&calls);
                    thread::spawn(move || {
                        transport.serve(request, crate::webui::shared_public_asset, |request| {
                            calls.fetch_add(1, Ordering::SeqCst);
                            if request.url() == "/api/broken" {
                                struct Broken;
                                impl Read for Broken {
                                    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                                        Err(io::Error::other("synthetic failure"))
                                    }
                                }
                                return Response::new(
                                    StatusCode(200),
                                    Vec::new(),
                                    Box::new(Broken) as Box<dyn Read + Send>,
                                    None,
                                    None,
                                );
                            }
                            if request.url() == "/api/large" {
                                return Response::new(
                                    StatusCode(200),
                                    Vec::new(),
                                    Box::new(io::repeat(73).take(8 * 1024 * 1024))
                                        as Box<dyn Read + Send>,
                                    Some(8 * 1024 * 1024),
                                    None,
                                );
                            }
                            assert_eq!(request.method(), &Method::Post);
                            assert_eq!(request.url(), "/api/private?path=secret-path");
                            assert!(request.headers().iter().any(|h| h.field.equiv("Cookie")
                                && h.value.as_str() == "token=secret-token"));
                            let mut body = Vec::new();
                            request.as_reader().read_to_end(&mut body).unwrap();
                            Response::from_data(body)
                                .with_status_code(StatusCode(201))
                                .with_header(
                                    Header::from_bytes("Set-Cookie", "token=new-secret-token")
                                        .unwrap(),
                                )
                                .with_header(
                                    Header::from_bytes(
                                        "Content-Disposition",
                                        "attachment; filename=secret-name.txt",
                                    )
                                    .unwrap(),
                                )
                                .boxed()
                        })
                    });
                }
            })
        };
        Self {
            endpoint,
            transport,
            stop,
            calls,
            worker: Some(worker),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}
fn head(body_length: usize) -> RequestHead {
    RequestHead {
        method: "POST".into(),
        url: "/api/private?path=secret-path".into(),
        headers: vec![("Cookie".into(), "token=secret-token".into())],
        body_length,
    }
}

#[test]
fn outer_http_hides_metadata_and_rejects_plaintext_replay_and_tampering() {
    let fixture = Fixture::start();
    let client = client();
    for path in [
        "/api/auth/status",
        "/api/snapshot",
        "/api/managed/ready",
        "/unknown",
    ] {
        assert_eq!(
            client
                .get(format!("{}{path}", fixture.endpoint))
                .send()
                .unwrap()
                .status(),
            426
        );
    }
    assert_eq!(
        client
            .get(format!("{}/app.js", fixture.endpoint))
            .send()
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        client
            .post(format!("{}/api/auth/login", fixture.endpoint))
            .body("secret-password")
            .send()
            .unwrap()
            .status(),
        426
    );
    let (state, hello) = me_transport::initiate().unwrap();
    let reply = client
        .post(format!(
            "{}{}",
            fixture.endpoint,
            me_transport::HANDSHAKE_PATH
        ))
        .body(hello)
        .send()
        .unwrap()
        .bytes()
        .unwrap();
    let channel = me_transport::finish(state, &reply[16..]).unwrap();
    let mut packet = reply[..16].to_vec();
    packet.extend(0u32.to_be_bytes());
    packet.extend(
        me_transport::encode_request(Arc::clone(&channel), 0, &head(16), b"secret-password!")
            .unwrap(),
    );
    let mut corrupt = packet.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    assert_eq!(
        client
            .post(format!(
                "{}{}",
                fixture.endpoint,
                me_transport::REQUEST_PATH
            ))
            .body(corrupt)
            .send()
            .unwrap()
            .status(),
        400
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    let response = client
        .post(format!(
            "{}{}",
            fixture.endpoint,
            me_transport::REQUEST_PATH
        ))
        .body(packet.clone())
        .send()
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.headers().get("Set-Cookie").is_none());
    assert!(response.headers().get("Content-Disposition").is_none());
    let encrypted = response.bytes().unwrap();
    for secret in [
        b"secret-password".as_slice(),
        b"secret-path",
        b"secret-token",
        b"secret-name",
    ] {
        assert!(!packet.windows(secret.len()).any(|v| v == secret));
        assert!(!encrypted.windows(secret.len()).any(|v| v == secret));
    }
    let (metadata, mut reader) =
        me_transport::DecryptReader::new(Cursor::new(encrypted), channel, 0).unwrap();
    let metadata: ResponseHead = serde_json::from_slice(&metadata).unwrap();
    assert_eq!(metadata.status, 201);
    assert!(
        metadata
            .headers
            .iter()
            .any(|(_, v)| v == "token=new-secret-token")
    );
    reader.set_expected_length(metadata.body_length);
    let mut body = Vec::new();
    reader.read_to_end(&mut body).unwrap();
    assert_eq!(body, b"secret-password!");
    assert_eq!(
        client
            .post(format!(
                "{}{}",
                fixture.endpoint,
                me_transport::REQUEST_PATH
            ))
            .body(packet)
            .send()
            .unwrap()
            .status(),
        400
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn blocking_client_concurrency_expiry_and_bounded_streaming() {
    let fixture = Fixture::start();
    let transport = Arc::new(me_transport::blocking::Transport::new(&fixture.endpoint));
    let client = client();
    let workers: Vec<_> = (0..24)
        .map(|_| {
            let transport = Arc::clone(&transport);
            let client = client.clone();
            thread::spawn(move || {
                let mut reply = transport.request(&client, &head(5), b"hello").unwrap();
                assert_eq!(reply.head.status, 201);
                let mut body = Vec::new();
                reply.body.read_to_end(&mut body).unwrap();
                assert_eq!(body, b"hello");
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 24);
    assert_eq!(fixture.transport.sessions.lock().unwrap().len(), 1);
    for session in fixture.transport.sessions.lock().unwrap().values_mut() {
        session.created = Instant::now() - CHANNEL_LIFETIME;
    }
    let mut reply = transport.request(&client, &head(5), b"hello").unwrap();
    io::copy(&mut reply.body, &mut io::sink()).unwrap();
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 25);
    let mut large = head(0);
    large.url = "/api/large".into();
    let mut response = transport.request(&client, &large, &[]).unwrap();
    let mut bytes = [0; 8192];
    let mut total = 0;
    loop {
        let n = response.body.read(&mut bytes).unwrap();
        if n == 0 {
            break;
        }
        assert!(bytes[..n].iter().all(|b| *b == 73));
        total += n;
    }
    assert_eq!(total, 8 * 1024 * 1024);
}

#[test]
fn truncated_response_never_reexecutes_request() {
    let fixture = Fixture::start();
    let transport = me_transport::blocking::Transport::new(&fixture.endpoint);
    let mut request = head(0);
    request.url = "/api/broken".into();
    match transport.request(&client(), &request, &[]) {
        Ok(mut response) => assert!(io::copy(&mut response.body, &mut io::sink()).is_err()),
        Err(_) => {}
    }
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn channel_capacity_is_bounded_and_expired_entries_are_reclaimed() {
    let transport = EncryptedHttp::default();
    let (_, hello) = me_transport::initiate().unwrap();
    let (channel, _) = me_transport::respond(&hello).unwrap();
    {
        let mut sessions = transport.sessions.lock().unwrap();
        for n in 0..MAX_CHANNELS {
            let mut id = [0; 16];
            id[..8].copy_from_slice(&(n as u64).to_be_bytes());
            sessions.insert(
                id,
                Session {
                    channel: Arc::clone(&channel),
                    created: Instant::now(),
                },
            );
        }
    }
    assert_eq!(
        transport.handshake(&hello).unwrap().status_code(),
        StatusCode(503)
    );
    for session in transport.sessions.lock().unwrap().values_mut() {
        session.created = Instant::now() - CHANNEL_LIFETIME;
    }
    assert_eq!(
        transport.handshake(&hello).unwrap().status_code(),
        StatusCode(200)
    );
    assert_eq!(transport.sessions.lock().unwrap().len(), 1);
}
