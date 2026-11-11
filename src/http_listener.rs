use std::{
    io,
    net::{Ipv6Addr, SocketAddrV6, TcpListener},
};

use socket2::{Domain, Protocol, Socket, Type};
use tiny_http::Server;

pub(crate) fn bind(port: u16) -> crate::Result<Vec<Server>> {
    bind_listeners(port, bind_ipv6)?
        .into_iter()
        .map(|listener| Server::from_listener(listener, None))
        .collect()
}

fn bind_listeners(
    port: u16,
    ipv6: impl FnOnce(u16) -> io::Result<TcpListener>,
) -> io::Result<Vec<TcpListener>> {
    let ipv4 = TcpListener::bind(("0.0.0.0", port))?;
    let mut listeners = vec![ipv4];
    match ipv6(listeners[0].local_addr()?.port()) {
        Ok(listener) => listeners.push(listener),
        Err(error) if ipv6_unavailable(&error) => {
            eprintln!("warning: IPv6 is unavailable; WebUI is listening on IPv4 only: {error}");
        }
        Err(error) => return Err(error),
    }
    Ok(listeners)
}

fn bind_ipv6(port: u16) -> io::Result<TcpListener> {
    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    // Separate address families make port ownership consistent across all supported OSes.
    socket.set_only_v6(true)?;
    socket.bind(&SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0).into())?;
    socket.listen(128)?;
    Ok(socket.into())
}

fn ipv6_unavailable(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::Unsupported | io::ErrorKind::AddrNotAvailable
    ) {
        return true;
    }
    #[cfg(unix)]
    let codes = [libc::EAFNOSUPPORT, libc::EPROTONOSUPPORT, libc::ENOPROTOOPT];
    // Winsock: WSAEAFNOSUPPORT, WSAEPROTONOSUPPORT, WSAENOPROTOOPT.
    #[cfg(windows)]
    let codes = [10047, 10043, 10042];
    error
        .raw_os_error()
        .is_some_and(|code| codes.contains(&code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpStream, thread, time::Duration};

    #[test]
    fn listeners_accept_both_address_families() {
        let listeners = bind_listeners(0, bind_ipv6).unwrap();
        assert_eq!(listeners.len(), 2);
        let port = listeners[0].local_addr().unwrap().port();
        assert_eq!(listeners[1].local_addr().unwrap().port(), port);
        for (listener, host) in listeners.iter().zip(["127.0.0.1", "[::1]"]) {
            let address = format!("{host}:{port}").parse().unwrap();
            let client = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
            let (connection, _) = listener.accept().unwrap();
            client.shutdown(std::net::Shutdown::Both).unwrap();
            drop(client);
            drop(connection);
        }
    }

    #[test]
    fn unused_listeners_release_both_ports() {
        let listeners = bind_listeners(0, bind_ipv6).unwrap();
        let port = listeners[0].local_addr().unwrap().port();
        drop(listeners);
        let _listeners = bind_listeners(port, bind_ipv6).unwrap();
    }

    #[test]
    fn conflicts_in_either_family_are_errors_and_release_partial_binds() {
        let ipv4 = TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let error = bind(ipv4.local_addr().unwrap().port()).err().unwrap();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::AddrInUse
        );
        let ipv6 = bind_ipv6(0).unwrap();
        let port = ipv6.local_addr().unwrap().port();
        let error = bind(port).err().unwrap();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::AddrInUse
        );
        let _released_ipv4 = TcpListener::bind(("0.0.0.0", port)).unwrap();
    }

    #[test]
    fn ipv6_unavailable_retains_ipv4_but_other_errors_fail() {
        let listeners = bind_listeners(0, |_| Err(io::ErrorKind::Unsupported.into())).unwrap();
        assert_eq!(listeners.len(), 1);
        assert!(listeners[0].local_addr().unwrap().is_ipv4());
        for kind in [io::ErrorKind::AddrInUse, io::ErrorKind::PermissionDenied] {
            let error = bind_listeners(0, |_| Err(kind.into())).unwrap_err();
            assert_eq!(error.kind(), kind);
        }
        #[cfg(unix)]
        assert!(ipv6_unavailable(&io::Error::from_raw_os_error(
            libc::EAFNOSUPPORT
        )));
        #[cfg(windows)]
        assert!(ipv6_unavailable(&io::Error::from_raw_os_error(10047)));
    }

    #[test]
    fn http_servers_serve_both_families_on_the_same_port() {
        let servers = bind(0).unwrap();
        assert_eq!(servers.len(), 2);
        let port = servers[0].server_addr().to_ip().unwrap().port();
        let ipv6_address = servers[1].server_addr().to_ip().unwrap();
        assert_eq!(
            format!("http://{ipv6_address}"),
            format!("http://[::]:{port}")
        );
        let workers: Vec<_> = servers
            .into_iter()
            .map(|server| {
                thread::spawn(move || {
                    server
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap()
                        .unwrap()
                        .respond(tiny_http::Response::from_string("dual-stack"))
                        .unwrap();
                })
            })
            .collect();
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        for host in ["127.0.0.1", "[::1]"] {
            let response = client.get(format!("http://{host}:{port}")).send().unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(response.text().unwrap(), "dual-stack");
        }
        for worker in workers {
            worker.join().unwrap();
        }
    }
}
