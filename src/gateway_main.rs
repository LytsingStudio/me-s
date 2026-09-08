use std::{env, thread, time::Duration};

use me::{Result, gateway::Gateway, gateway_webui, termination::TerminationSignals, updater};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    #[cfg(windows)]
    if updater::run_windows_helper_if_requested()? {
        return Ok(());
    }
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments.as_slice() == ["version"] {
        println!("me-gateway {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if arguments.as_slice() == ["update"] {
        updater::update()?;
        return Ok(());
    }
    let options = parse_options(&arguments)?;
    me::process_limits::prepare();
    let root = env::current_dir()?;
    let gateway = Gateway::start(&root)?;
    let server = gateway_webui::start(
        Arc::clone(&gateway),
        options.passkey.as_deref(),
        options.port,
    )?;
    eprintln!("ME Gateway: {}", server.address());
    let termination = TerminationSignals::install()?;
    let mut failure = None;
    while !termination.requested() {
        if let Err(error) = gateway.poll() {
            failure = Some(error);
            break;
        }
        thread::park_timeout(Duration::from_millis(200));
    }
    drop(server);
    gateway.shutdown();
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(())
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Options {
    passkey: Option<String>,
    port: Option<u16>,
}

fn parse_options(arguments: &[String]) -> Result<Options> {
    let mut options = Options::default();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--webui-passkey" => {
                if options.passkey.is_some() {
                    return Err("--webui-passkey may only be specified once".into());
                }
                let value = arguments
                    .get(index + 1)
                    .ok_or("--webui-passkey requires a password")?;
                if value.is_empty() {
                    return Err("--webui-passkey password must not be empty".into());
                }
                options.passkey = Some(value.clone());
                index += 2;
            }
            "--webui-port" => {
                if options.port.is_some() {
                    return Err("--webui-port may only be specified once".into());
                }
                let value = arguments
                    .get(index + 1)
                    .ok_or("--webui-port requires a port")?;
                options.port = Some(
                    value
                        .parse::<u16>()
                        .ok()
                        .filter(|port| *port != 0)
                        .ok_or("--webui-port must be an integer from 1 to 65535")?,
                );
                index += 2;
            }
            argument => return Err(format!("unknown me-gateway option: {argument}").into()),
        }
    }
    Ok(options)
}

use std::sync::Arc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_options_accept_only_one_nonempty_passkey() {
        assert_eq!(parse_options(&[]).unwrap(), Options::default());
        assert_eq!(
            parse_options(&["--webui-passkey".into(), "secret".into()]).unwrap(),
            Options {
                passkey: Some("secret".into()),
                port: None
            }
        );
        assert!(parse_options(&["--webui-passkey".into()]).is_err());
        assert!(parse_options(&["--unknown".into()]).is_err());
    }

    #[test]
    fn gateway_port_is_explicit_nonzero_and_composable() {
        let args = ["--webui-passkey", "secret", "--webui-port", "65535"].map(String::from);
        assert_eq!(
            parse_options(&args).unwrap(),
            Options {
                passkey: Some("secret".into()),
                port: Some(65535)
            }
        );
        for value in ["0", "65536", "-1", "abc", ""] {
            assert!(parse_options(&["--webui-port".into(), value.into()]).is_err());
        }
        assert!(parse_options(&["--webui-port".into()]).is_err());
        assert!(
            parse_options(&["--webui-port", "39001", "--webui-port", "39002"].map(String::from))
                .is_err()
        );
    }
}
