use std::{
    env,
    io::Write,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::{
    Result,
    managed_protocol::{
        MANAGED_PROTOCOL_VERSION, MANAGED_READY_PATH, MANAGED_SHUTDOWN_PATH, ManagedLaunchConfig,
        ManagedReadyResponse, bearer_header_value, random_hex_secret,
    },
};

const START_ATTEMPTS: usize = 5;
const READY_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct ProcessRoute {
    pub address: String,
    pub token: String,
}

pub struct ManagedProcess {
    child: Child,
    input: Option<ChildStdin>,
    route: ProcessRoute,
    instance_nonce: String,
    workspace_path: PathBuf,
}

impl ManagedProcess {
    pub fn start(me_s: &Path, workspace: &Path) -> Result<Self> {
        if !me_s.is_file() {
            return Err(format!("me-s executable was not found at {}", me_s.display()).into());
        }
        let canonical_workspace = std::fs::canonicalize(workspace)?;
        if !canonical_workspace.is_dir() {
            return Err(format!("Workspace is not a directory: {}", workspace.display()).into());
        }
        let mut last_error = None;
        for _ in 0..START_ATTEMPTS {
            match Self::start_once(me_s, &canonical_workspace) {
                Ok(process) => return Ok(process),
                Err(error) => {
                    last_error = Some(error);
                    thread::sleep(Duration::from_millis(50));
                }
            }
        }
        Err(format!(
            "unable to start Workspace after {START_ATTEMPTS} attempts: {}",
            last_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "unknown startup failure".into())
        )
        .into())
    }

    fn start_once(me_s: &Path, workspace: &Path) -> Result<Self> {
        let port = available_port()?;
        let launch = ManagedLaunchConfig {
            protocol_version: MANAGED_PROTOCOL_VERSION,
            port,
            token: random_hex_secret(32)?,
            instance_nonce: random_hex_secret(16)?,
        };
        let mut command = Command::new(me_s);
        command
            .arg("__gateway-child")
            .current_dir(workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        let mut child = command.spawn()?;
        let mut input = child
            .stdin
            .take()
            .ok_or("managed me-s stdin pipe is unavailable")?;
        serde_json::to_writer(&mut input, &launch)?;
        input.write_all(b"\n")?;
        input.flush()?;
        let mut process = Self {
            child,
            input: Some(input),
            route: ProcessRoute {
                address: format!("http://127.0.0.1:{port}"),
                token: launch.token,
            },
            instance_nonce: launch.instance_nonce,
            workspace_path: workspace.to_owned(),
        };
        process.wait_ready()?;
        Ok(process)
    }

    fn wait_ready(&mut self) -> Result<()> {
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_millis(300))
            .timeout(Duration::from_millis(750))
            .build()?;
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Err(format!("managed me-s stopped during startup with {status}").into());
            }
            if let Ok(response) = client
                .get(format!("{}{}", self.route.address, MANAGED_READY_PATH))
                .header(
                    reqwest::header::AUTHORIZATION,
                    bearer_header_value(&self.route.token),
                )
                .send()
                && response.status().is_success()
            {
                let ready: ManagedReadyResponse = response.json()?;
                self.verify_ready(&ready)?;
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("managed me-s readiness timed out".into());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn verify_ready(&self, ready: &ManagedReadyResponse) -> Result<()> {
        if !ready.ok || !ready.ready {
            return Err("managed me-s did not report ready".into());
        }
        if ready.protocol_version != MANAGED_PROTOCOL_VERSION {
            return Err(format!(
                "managed protocol mismatch: expected {MANAGED_PROTOCOL_VERSION}, received {}",
                ready.protocol_version
            )
            .into());
        }
        if ready.product_version != env!("CARGO_PKG_VERSION") {
            return Err(format!(
                "me product version mismatch: gateway {}, Workspace {}",
                env!("CARGO_PKG_VERSION"),
                ready.product_version
            )
            .into());
        }
        if ready.instance_nonce != self.instance_nonce {
            return Err("managed me-s instance identity did not match".into());
        }
        if Path::new(&ready.workspace_path) != self.workspace_path {
            return Err("managed me-s Workspace path did not match".into());
        }
        Ok(())
    }

    pub fn route(&self) -> ProcessRoute {
        self.route.clone()
    }

    pub fn has_exited(&mut self) -> Result<bool> {
        Ok(self.child.try_wait()?.is_some())
    }

    pub fn shutdown(&mut self) {
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_millis(300))
            .timeout(Duration::from_secs(2))
            .build();
        if let Ok(client) = client {
            let _ = client
                .post(format!("{}{}", self.route.address, MANAGED_SHUTDOWN_PATH))
                .header(
                    reqwest::header::AUTHORIZATION,
                    bearer_header_value(&self.route.token),
                )
                .send();
        }
        self.input.take();
        let deadline = Instant::now() + STOP_TIMEOUT;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => thread::sleep(Duration::from_millis(50)),
                Err(_) => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub fn me_s_executable() -> Result<PathBuf> {
    if let Some(path) = env::var_os("ME_GATEWAY_ME_S") {
        return Ok(PathBuf::from(path));
    }
    let current = env::current_exe()?;
    let directory = current
        .parent()
        .ok_or("me-gateway executable has no parent directory")?;
    #[cfg(windows)]
    let name = "me-s.exe";
    #[cfg(not(windows))]
    let name = "me-s";
    Ok(directory.join(name))
}

fn available_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}
