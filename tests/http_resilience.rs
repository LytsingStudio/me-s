#![cfg(unix)]

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tiny_http::{Response, Server};

struct TestChild(Child);
impl Drop for TestChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn nofile() -> libc::rlimit {
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) },
        0
    );
    limits
}

fn pressure_child() {
    let original = nofile();
    let low = libc::rlimit {
        rlim_cur: 128,
        rlim_max: original.rlim_max,
    };
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &low) }, 0);
    me::process_limits::prepare();
    let raised = nofile();
    assert_eq!(raised.rlim_cur, 65_536.min(original.rlim_max));
    assert_eq!(raised.rlim_max, original.rlim_max);
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &low) }, 0);

    let server = Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    println!("ME_HTTP_TEST_PORT={port}");
    std::io::stdout().flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut saw_pressure = false;
    while Instant::now() < deadline {
        match server.recv_timeout(Duration::from_millis(100)) {
            Err(error) => {
                assert_eq!(error.raw_os_error(), Some(libc::EMFILE));
                saw_pressure = true;
            }
            Ok(Some(request)) => {
                assert!(saw_pressure, "the test must actually exhaust descriptors");
                assert_eq!(request.url(), "/finish");
                request.respond(Response::from_string("recovered")).unwrap();
                return;
            }
            Ok(None) => {}
        }
    }
    panic!("listener did not recover after descriptor pressure");
}

#[test]
fn descriptor_pressure_recovers() {
    const CHILD: &str = "ME_HTTP_PRESSURE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        pressure_child();
        return;
    }
    // RLIMIT is process-wide: never lower the parent test runner's limit.
    let mut child = TestChild(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "descriptor_pressure_recovers",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    let port = loop {
        let mut line = String::new();
        assert_ne!(
            output.read_line(&mut line).unwrap(),
            0,
            "child failed before readiness"
        );
        if let Some((_, port)) = line.split_once("ME_HTTP_TEST_PORT=") {
            break port.trim().parse::<u16>().unwrap();
        }
    };
    assert_ne!(port, 38200);
    let address = format!("127.0.0.1:{port}").parse().unwrap();
    let mut held = Vec::new();
    for _ in 0..80 {
        match TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
            Ok(socket) => held.push(socket),
            Err(_) => break,
        }
    }
    thread::sleep(Duration::from_millis(500));
    drop(held);
    thread::sleep(Duration::from_millis(1200));
    let mut socket = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    socket
        .write_all(b"GET /finish HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("recovered"));
    let mut remainder = String::new();
    output.read_to_string(&mut remainder).unwrap();
    assert!(child.0.wait().unwrap().success(), "{remainder}");
}

#[test]
fn incomplete_connection_expires_without_stopping_listener() {
    let server = Server::http("127.0.0.1:0").unwrap();
    let address = server.server_addr().to_ip().unwrap();
    assert_ne!(address.port(), 38200);
    let mut idle = TcpStream::connect(address).unwrap();
    idle.set_read_timeout(Some(Duration::from_secs(35)))
        .unwrap();
    idle.write_all(b"GET / HTTP/1.1\r\nHost:").unwrap();
    let started = Instant::now();
    let mut response = String::new();
    idle.read_to_string(&mut response).unwrap();
    assert!(started.elapsed() >= Duration::from_secs(25));
    assert!(response.is_empty() || response.starts_with("HTTP/1.1 408"));
    assert!(server.try_recv().unwrap().is_none());

    let mut next = TcpStream::connect(address).unwrap();
    next.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    next.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    server
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap()
        .respond(Response::from_string("still listening"))
        .unwrap();
    response.clear();
    next.read_to_string(&mut response).unwrap();
    assert!(response.ends_with("still listening"));
}
