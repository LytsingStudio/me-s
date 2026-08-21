use std::{env, thread, time::Duration};

use me::{Result, gateway::Gateway, gateway_webui, termination::TerminationSignals, updater};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments.as_slice() == ["version"] {
        println!("me-gateway {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if arguments.as_slice() == ["update"] {
        updater::update()?;
        return Ok(());
    }
    let passkey = parse_options(&arguments)?;
    let root = env::current_dir()?;
    let gateway = Gateway::start(&root)?;
    let server = gateway_webui::start(Arc::clone(&gateway), passkey.as_deref())?;
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

fn parse_options(arguments: &[String]) -> Result<Option<String>> {
    let mut passkey = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--webui-passkey" => {
                if passkey.is_some() {
                    return Err("--webui-passkey may only be specified once".into());
                }
                let value = arguments
                    .get(index + 1)
                    .ok_or("--webui-passkey requires a password")?;
                if value.is_empty() {
                    return Err("--webui-passkey password must not be empty".into());
                }
                passkey = Some(value.clone());
                index += 2;
            }
            argument => return Err(format!("unknown me-gateway option: {argument}").into()),
        }
    }
    Ok(passkey)
}

use std::sync::Arc;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_options_accept_only_one_nonempty_passkey() {
        assert_eq!(parse_options(&[]).unwrap(), None);
        assert_eq!(
            parse_options(&["--webui-passkey".into(), "secret".into()]).unwrap(),
            Some("secret".into())
        );
        assert!(parse_options(&["--webui-passkey".into()]).is_err());
        assert!(parse_options(&["--unknown".into()]).is_err());
    }
}
