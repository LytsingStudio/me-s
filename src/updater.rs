use std::{
    env,
    error::Error as StdError,
    fs,
    fs::File,
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

use sha2::{Digest, Sha256};

use crate::Result;

pub const RELEASE_REPOSITORY: &str = "LytsingStudio/me-s";
const CHECKSUM_ASSET: &str = "SHA256SUMS";
const UPDATE_USER_AGENT: &str = concat!("me/", env!("CARGO_PKG_VERSION"));
const METADATA_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const METADATA_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const DOWNLOAD_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[cfg(any(windows, test))]
const WINDOWS_UPDATE_POWERSHELL_ARGS: [&str; 3] = ["-NoLogo", "-NoProfile", "-NonInteractive"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UpdatePlatform {
    me_s_asset: &'static str,
    gateway_asset: &'static str,
    me_s_executable: &'static str,
    gateway_executable: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PublicRelease {
    tag_name: String,
}

impl UpdatePlatform {
    fn detect() -> Result<Self> {
        Self::for_target(env::consts::OS, env::consts::ARCH)
    }

    fn for_target(os: &str, arch: &str) -> Result<Self> {
        let (me_s_asset, gateway_asset, me_s_executable, gateway_executable) = match (os, arch) {
            ("macos", "aarch64") => (
                "me-s-macos-arm64",
                "me-gateway-macos-arm64",
                "me-s",
                "me-gateway",
            ),
            ("macos", "x86_64") => (
                "me-s-macos-x86_64",
                "me-gateway-macos-x86_64",
                "me-s",
                "me-gateway",
            ),
            ("linux", "aarch64") => (
                "me-s-linux-arm64",
                "me-gateway-linux-arm64",
                "me-s",
                "me-gateway",
            ),
            ("linux", "x86_64") => (
                "me-s-linux-x86_64",
                "me-gateway-linux-x86_64",
                "me-s",
                "me-gateway",
            ),
            ("windows", "x86_64") => (
                "me-s-windows-x86_64.exe",
                "me-gateway-windows-x86_64.exe",
                "me-s.exe",
                "me-gateway.exe",
            ),
            _ => return Err(format!("ME update does not support {os}/{arch}").into()),
        };
        Ok(Self {
            me_s_asset,
            gateway_asset,
            me_s_executable,
            gateway_executable,
        })
    }

    fn assets(self) -> [&'static str; 2] {
        [self.me_s_asset, self.gateway_asset]
    }

    fn executable_names(self) -> [&'static str; 2] {
        [self.me_s_executable, self.gateway_executable]
    }
}

pub fn update() -> Result<()> {
    let platform = UpdatePlatform::detect()?;
    let running = env::current_exe()
        .map_err(|error| format!("cannot locate the running ME executable: {error}"))?;
    let install_directory = running
        .parent()
        .ok_or("the running ME executable has no installation directory")?;
    let executable_names = platform.executable_names();
    let destinations = [
        install_directory.join(executable_names[0]),
        install_directory.join(executable_names[1]),
    ];

    let metadata_client = update_metadata_client()?;
    let release = latest_release(&metadata_client)?;
    let latest_tag = release.tag_name.as_str();
    let current_tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    if latest_tag == current_tag
        && installed_product_matches(&destinations, env!("CARGO_PKG_VERSION"))
    {
        println!("ME is already up to date: {current_tag}");
        return Ok(());
    }
    if release_version(latest_tag)? < release_version(&current_tag)? {
        println!(
            "the running ME version {current_tag} is newer than the latest published release {latest_tag}; no update was installed"
        );
        return Ok(());
    }

    if latest_tag == current_tag {
        println!("repairing the incomplete ME {current_tag} installation");
    } else {
        println!("updating ME: {current_tag} -> {latest_tag}");
    }
    let download_client = update_download_client()?;
    let temporary = UpdateTempDirectory::create()?;
    let assets = platform.assets();
    download_release(&download_client, &release, &assets, temporary.path())?;

    let checksums = temporary.path().join(CHECKSUM_ASSET);
    let downloaded = [
        temporary.path().join(assets[0]),
        temporary.path().join(assets[1]),
    ];
    for (path, asset) in downloaded.iter().zip(assets) {
        verify_release_asset(path, &checksums, asset)?;
    }
    let expected_version = latest_tag.strip_prefix('v').unwrap_or(latest_tag);
    validate_downloaded_product(&downloaded, expected_version)?;

    let scheduled_after_exit = deploy_product(&downloaded, &destinations)?;
    if scheduled_after_exit {
        let error_log = install_directory.join(".me-update-error.log");
        println!(
            "downloaded and verified ME {latest_tag}; Windows will install both programs after this process exits\nme-s: {}\nme-gateway: {}\nerror log if installation cannot be completed: {}\nglobal configuration: unchanged",
            destinations[0].display(),
            destinations[1].display(),
            error_log.display()
        );
    } else {
        println!(
            "updated ME to {latest_tag}\nme-s: {}\nme-gateway: {}\nglobal configuration: unchanged",
            destinations[0].display(),
            destinations[1].display()
        );
    }
    Ok(())
}

fn installed_product_matches(destinations: &[PathBuf; 2], version: &str) -> bool {
    for (destination, program) in destinations.iter().zip(["me-s", "me-gateway"]) {
        let Ok(metadata) = fs::symlink_metadata(destination) else {
            return false;
        };
        if !metadata.file_type().is_file() {
            return false;
        }
        let Ok(output) = Command::new(destination).arg("version").output() else {
            return false;
        };
        if !output.status.success() {
            return false;
        }
        let expected = format!("{program} {version}");
        if String::from_utf8_lossy(&output.stdout).trim() != expected {
            return false;
        }
    }
    true
}

fn validate_downloaded_product(programs: &[PathBuf; 2], version: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for program in programs {
            fs::set_permissions(program, fs::Permissions::from_mode(0o755))?;
        }
    }
    for (program, name) in programs.iter().zip(["me-s", "me-gateway"]) {
        let output = Command::new(program)
            .arg("version")
            .output()
            .map_err(|error| {
                format!("downloaded {name} could not be executed before installation: {error}")
            })?;
        if !output.status.success() {
            return Err(format!(
                "downloaded {name} version check failed with {}",
                output.status
            )
            .into());
        }
        let expected = format!("{name} {version}");
        let actual = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if actual != expected {
            return Err(format!(
                "downloaded {name} reported an unexpected version: expected {expected}, received {actual}"
            )
            .into());
        }
    }
    Ok(())
}

fn update_metadata_client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .connect_timeout(METADATA_CONNECT_TIMEOUT)
        .timeout(METADATA_REQUEST_TIMEOUT)
        .user_agent(UPDATE_USER_AGENT)
        .build()?)
}

fn update_download_client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .connect_timeout(DOWNLOAD_CONNECT_TIMEOUT)
        .timeout(DOWNLOAD_REQUEST_TIMEOUT)
        .user_agent(UPDATE_USER_AGENT)
        .build()?)
}

fn latest_release(client: &reqwest::blocking::Client) -> Result<PublicRelease> {
    let public_url = format!("https://github.com/{RELEASE_REPOSITORY}/releases/latest");
    let api_url = format!("https://api.github.com/repos/{RELEASE_REPOSITORY}/releases/latest");
    latest_release_from_sources(client, &public_url, &api_url)
}

fn latest_release_from_sources(
    client: &reqwest::blocking::Client,
    public_url: &str,
    api_url: &str,
) -> Result<PublicRelease> {
    let public_error = match latest_release_from_url(client, public_url) {
        Ok(release) => return Ok(release),
        Err(error) => error,
    };
    match latest_release_from_api(client, api_url) {
        Ok(release) => Ok(release),
        Err(api_error) => Err(format!(
            "cannot query the latest public me-s release; public redirect failed: {public_error}; GitHub API fallback failed: {api_error}"
        )
        .into()),
    }
}

fn latest_release_from_url(client: &reqwest::blocking::Client, url: &str) -> Result<PublicRelease> {
    let response = client
        .get(url)
        .send()
        .map_err(|error| request_error("public release redirect", &error))?;
    let status = response.status();
    if !status.is_success() {
        let detail = response.text().unwrap_or_default();
        return Err(format!(
            "cannot query the latest public me-s release: HTTP {status}{}",
            response_detail(&detail)
        )
        .into());
    }
    let tag_name = release_tag_from_url(response.url())?;
    Ok(PublicRelease { tag_name })
}

fn latest_release_from_api(client: &reqwest::blocking::Client, url: &str) -> Result<PublicRelease> {
    let response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(|error| request_error("GitHub release API", &error))?;
    let status = response.status();
    if !status.is_success() {
        let detail = response.text().unwrap_or_default();
        return Err(format!("HTTP {status}{}", response_detail(&detail)).into());
    }
    let body = response
        .json::<serde_json::Value>()
        .map_err(|error| format!("invalid GitHub release response: {error}"))?;
    let tag_name = body
        .get("tag_name")
        .and_then(serde_json::Value::as_str)
        .filter(|tag| !tag.is_empty())
        .ok_or("GitHub release response has no tag_name")?;
    release_version(tag_name)?;
    Ok(PublicRelease {
        tag_name: tag_name.to_owned(),
    })
}

fn request_error(operation: &str, error: &reqwest::Error) -> String {
    let mut details = vec![error.to_string()];
    let mut source = error.source();
    while let Some(error) = source {
        let detail = error.to_string();
        if !detail.is_empty() && details.last() != Some(&detail) {
            details.push(detail);
        }
        source = error.source();
    }
    format!("{operation}: {}", details.join(": "))
}

fn download_release(
    client: &reqwest::blocking::Client,
    release: &PublicRelease,
    assets: &[&str],
    directory: &Path,
) -> Result<()> {
    for name in assets
        .iter()
        .copied()
        .chain(std::iter::once(CHECKSUM_ASSET))
    {
        let url = release_asset_url(release, name)?;
        download_asset(client, name, url.as_str(), &directory.join(name))?;
    }
    Ok(())
}

fn release_tag_from_url(url: &reqwest::Url) -> Result<String> {
    let segments = url
        .path_segments()
        .ok_or_else(|| format!("latest release redirected to an invalid URL: {url}"))?
        .collect::<Vec<_>>();
    let tag = segments
        .windows(3)
        .find(|parts| parts[0] == "releases" && parts[1] == "tag")
        .map(|parts| parts[2])
        .filter(|tag| !tag.is_empty())
        .ok_or_else(|| format!("latest release did not redirect to a versioned release: {url}"))?;
    release_version(tag)?;
    Ok(tag.to_owned())
}

fn release_asset_url(release: &PublicRelease, name: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&format!(
        "https://github.com/{RELEASE_REPOSITORY}/releases/download/"
    ))?;
    url.path_segments_mut()
        .map_err(|_| "cannot construct the public release download URL")?
        .pop_if_empty()
        .push(&release.tag_name)
        .push(name);
    Ok(url)
}

fn download_asset(
    client: &reqwest::blocking::Client,
    name: &str,
    url: &str,
    destination: &Path,
) -> Result<()> {
    let mut response = client
        .get(url)
        .send()
        .map_err(|error| format!("cannot download release asset {name}: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        let detail = response.text().unwrap_or_default();
        return Err(format!(
            "cannot download release asset {name}: HTTP {status}{}",
            response_detail(&detail)
        )
        .into());
    }
    let mut file = File::create(destination)?;
    std::io::copy(&mut response, &mut file)
        .map_err(|error| format!("cannot save release asset {name}: {error}"))?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn response_detail(body: &str) -> String {
    let detail = body.trim().chars().take(512).collect::<String>();
    if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    }
}

fn release_version(tag: &str) -> Result<(u64, u64, u64)> {
    let core = tag
        .strip_prefix('v')
        .unwrap_or(tag)
        .split(['-', '+'])
        .next()
        .unwrap_or_default();
    let values = core
        .split('.')
        .map(str::parse::<u64>)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| format!("invalid me-s release tag: {tag}"))?;
    let [major, minor, patch] = values.as_slice() else {
        return Err(format!("invalid me-s release tag: {tag}").into());
    };
    Ok((*major, *minor, *patch))
}

fn verify_release_asset(executable: &Path, manifest: &Path, asset: &str) -> Result<()> {
    if !executable.is_file() {
        return Err(format!("release asset was not downloaded: {asset}").into());
    }
    if executable.metadata()?.len() == 0 {
        return Err(format!("downloaded release asset is empty: {asset}").into());
    }
    let manifest = fs::read_to_string(manifest)
        .map_err(|error| format!("cannot read downloaded {CHECKSUM_ASSET}: {error}"))?;
    let expected = checksum_for_asset(&manifest, asset)?;
    let actual = sha256_file(executable)?;
    if actual != expected {
        return Err(format!(
            "release checksum mismatch for {asset}: expected {expected}, received {actual}"
        )
        .into());
    }
    Ok(())
}

fn checksum_for_asset(manifest: &str, asset: &str) -> Result<String> {
    let mut found = None;
    for line in manifest.lines() {
        let mut fields = line.split_whitespace();
        let Some(checksum) = fields.next() else {
            continue;
        };
        let Some(file) = fields.next() else {
            continue;
        };
        let file = file.strip_prefix('*').unwrap_or(file);
        if file != asset {
            continue;
        }
        if fields.next().is_some() {
            return Err(format!("{CHECKSUM_ASSET} contains an invalid entry for {asset}").into());
        }
        if found.is_some() {
            return Err(format!("{CHECKSUM_ASSET} contains duplicate entries for {asset}").into());
        }
        let checksum = checksum.to_ascii_lowercase();
        if checksum.len() != 64 || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(
                format!("{CHECKSUM_ASSET} contains an invalid checksum for {asset}").into(),
            );
        }
        found = Some(checksum);
    }
    found.ok_or_else(|| format!("{CHECKSUM_ASSET} does not contain {asset}").into())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(unix)]
fn deploy_product(downloaded: &[PathBuf; 2], destinations: &[PathBuf; 2]) -> Result<bool> {
    match atomic_install_product_unix(downloaded, destinations) {
        Ok(()) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!(
                "write permission is required for {}; requesting sudo",
                destinations[0]
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .display()
            );
            privileged_install_product_unix(downloaded, destinations)?;
            Ok(false)
        }
        Err(error) => Err(format!("cannot install the ME product update: {error}").into()),
    }
}

#[cfg(unix)]
fn atomic_install_product_unix(
    downloaded: &[PathBuf; 2],
    destinations: &[PathBuf; 2],
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let staging = [
        sibling_temporary_path(&destinations[0], "update")?,
        sibling_temporary_path(&destinations[1], "update")?,
    ];
    let backups = [
        sibling_temporary_path(&destinations[0], "backup")?,
        sibling_temporary_path(&destinations[1], "backup")?,
    ];
    let stage_result = (|| {
        for index in 0..2 {
            fs::copy(&downloaded[index], &staging[index])?;
            fs::set_permissions(&staging[index], fs::Permissions::from_mode(0o755))?;
            File::open(&staging[index])?.sync_all()?;
        }
        Ok(())
    })();
    if let Err(error) = stage_result {
        for path in &staging {
            let _ = fs::remove_file(path);
        }
        return Err(error);
    }
    commit_staged_product_unix(&staging, destinations, &backups)
}

#[cfg(unix)]
fn commit_staged_product_unix(
    staging: &[PathBuf; 2],
    destinations: &[PathBuf; 2],
    backups: &[PathBuf; 2],
) -> std::io::Result<()> {
    let mut had_original = [false; 2];
    let mut installed = [false; 2];
    let result = (|| {
        for index in 0..2 {
            match fs::symlink_metadata(&destinations[index]) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    fs::rename(&destinations[index], &backups[index])?;
                    had_original[index] = true;
                }
                Ok(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!(
                            "update target is not a regular file: {}",
                            destinations[index].display()
                        ),
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        for index in 0..2 {
            fs::rename(&staging[index], &destinations[index])?;
            installed[index] = true;
        }
        Ok(())
    })();

    if let Err(error) = result {
        let rollback =
            rollback_product_unix(destinations, staging, backups, &had_original, &installed);
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(std::io::Error::other(format!(
                "{error}; rollback also failed: {rollback_error}"
            ))),
        };
    }

    for index in 0..2 {
        if had_original[index] {
            if let Err(error) = fs::remove_file(&backups[index]) {
                eprintln!(
                    "warning: updated ME but could not remove backup {}: {error}",
                    backups[index].display()
                );
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn rollback_product_unix(
    destinations: &[PathBuf; 2],
    staging: &[PathBuf; 2],
    backups: &[PathBuf; 2],
    had_original: &[bool; 2],
    installed: &[bool; 2],
) -> std::io::Result<()> {
    let mut errors = Vec::new();
    for index in (0..2).rev() {
        if installed[index] && destinations[index].exists() {
            if let Err(error) = fs::remove_file(&destinations[index]) {
                errors.push(format!(
                    "cannot remove {}: {error}",
                    destinations[index].display()
                ));
            }
        }
        if had_original[index] && backups[index].exists() {
            if let Err(error) = fs::rename(&backups[index], &destinations[index]) {
                errors.push(format!(
                    "cannot restore {}: {error}",
                    destinations[index].display()
                ));
            }
        }
        if staging[index].exists() {
            if let Err(error) = fs::remove_file(&staging[index]) {
                errors.push(format!(
                    "cannot remove {}: {error}",
                    staging[index].display()
                ));
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(std::io::Error::other(errors.join("; ")))
    }
}

#[cfg(unix)]
fn privileged_install_product_unix(
    downloaded: &[PathBuf; 2],
    destinations: &[PathBuf; 2],
) -> Result<()> {
    let staging = [
        sibling_temporary_path(&destinations[0], "update")?,
        sibling_temporary_path(&destinations[1], "update")?,
    ];
    let backups = [
        sibling_temporary_path(&destinations[0], "backup")?,
        sibling_temporary_path(&destinations[1], "backup")?,
    ];
    let mut had_original = [false; 2];
    let mut installed = [false; 2];

    for destination in destinations {
        match fs::symlink_metadata(destination) {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Ok(_) => {
                return Err(format!(
                    "update target is not a regular file: {}",
                    destination.display()
                )
                .into());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }

    let result = (|| -> Result<()> {
        for index in 0..2 {
            run_checked(
                Command::new("sudo")
                    .arg("install")
                    .arg("-m")
                    .arg("755")
                    .arg(&downloaded[index])
                    .arg(&staging[index]),
                "stage the downloaded ME update",
            )?;
        }
        for index in 0..2 {
            if destinations[index].exists() {
                run_checked(
                    Command::new("sudo")
                        .arg("mv")
                        .arg("-f")
                        .arg(&destinations[index])
                        .arg(&backups[index]),
                    "back up the installed ME program",
                )?;
                had_original[index] = true;
            }
        }
        for index in 0..2 {
            run_checked(
                Command::new("sudo")
                    .arg("mv")
                    .arg("-f")
                    .arg(&staging[index])
                    .arg(&destinations[index]),
                "install the ME program update",
            )?;
            installed[index] = true;
        }
        Ok(())
    })();

    if let Err(error) = result {
        let mut rollback_errors = Vec::new();
        for index in (0..2).rev() {
            if installed[index] {
                if let Err(rollback) = run_checked(
                    Command::new("sudo")
                        .arg("rm")
                        .arg("-f")
                        .arg(&destinations[index]),
                    "remove a partially installed ME program",
                ) {
                    rollback_errors.push(rollback.to_string());
                }
            }
            if had_original[index] {
                if let Err(rollback) = run_checked(
                    Command::new("sudo")
                        .arg("mv")
                        .arg("-f")
                        .arg(&backups[index])
                        .arg(&destinations[index]),
                    "restore the previous ME program",
                ) {
                    rollback_errors.push(rollback.to_string());
                }
            }
            let _ = run_checked(
                Command::new("sudo")
                    .arg("rm")
                    .arg("-f")
                    .arg(&staging[index]),
                "remove an update staging file",
            );
        }
        if rollback_errors.is_empty() {
            return Err(error);
        }
        return Err(format!(
            "{error}; rollback also failed: {}",
            rollback_errors.join("; ")
        )
        .into());
    }

    for index in 0..2 {
        if had_original[index] {
            if let Err(error) = run_checked(
                Command::new("sudo")
                    .arg("rm")
                    .arg("-f")
                    .arg(&backups[index]),
                "remove an obsolete ME backup",
            ) {
                eprintln!("warning: {error}");
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn run_checked(command: &mut Command, operation: &str) -> Result<()> {
    let status = command
        .status()
        .map_err(|error| format!("cannot {operation}: {error}"))?;
    if !status.success() {
        return Err(format!("cannot {operation}: command exited with {status}").into());
    }
    Ok(())
}

#[cfg(windows)]
fn deploy_product(downloaded: &[PathBuf; 2], destinations: &[PathBuf; 2]) -> Result<bool> {
    let staging = [
        sibling_temporary_path(&destinations[0], "update")?,
        sibling_temporary_path(&destinations[1], "update")?,
    ];
    let backups = [
        sibling_temporary_path(&destinations[0], "backup")?,
        sibling_temporary_path(&destinations[1], "backup")?,
    ];
    let error_log = destinations[0]
        .parent()
        .ok_or("the Windows ME installation has no parent directory")?
        .join(".me-update-error.log");
    let _ = fs::remove_file(&error_log);
    for index in 0..2 {
        if let Err(error) = fs::copy(&downloaded[index], &staging[index]) {
            for path in &staging {
                let _ = fs::remove_file(path);
            }
            return Err(format!(
                "cannot stage the Windows update beside {}: {error}; run ME from an elevated terminal if it is installed in a protected directory",
                destinations[index].display()
            )
            .into());
        }
    }

    let values = [
        powershell_literal(&staging[0]),
        powershell_literal(&destinations[0]),
        powershell_literal(&backups[0]),
        powershell_literal(&staging[1]),
        powershell_literal(&destinations[1]),
        powershell_literal(&backups[1]),
        powershell_literal(&error_log),
    ];
    let script = windows_update_script(
        std::process::id(),
        &values[0],
        &values[1],
        &values[2],
        &values[3],
        &values[4],
        &values[5],
        &values[6],
    );
    let mut helper = Command::new("powershell.exe");
    helper
        .args(WINDOWS_UPDATE_POWERSHELL_ARGS)
        .arg("-Command")
        .arg(&script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    if let Err(error) = helper.spawn() {
        for path in &staging {
            let _ = fs::remove_file(path);
        }
        return Err(format!("cannot start the Windows update helper: {error}").into());
    }
    Ok(true)
}

#[cfg(any(windows, test))]
#[allow(clippy::too_many_arguments)]
fn windows_update_script(
    process_id: u32,
    source_me_s: &str,
    target_me_s: &str,
    backup_me_s: &str,
    source_gateway: &str,
    target_gateway: &str,
    backup_gateway: &str,
    error_log: &str,
) -> String {
    format!(
        "$ErrorActionPreference='Stop'; Wait-Process -Id {process_id} -ErrorAction SilentlyContinue; \
         function Move-WithRetry([string]$From,[string]$To) {{ for($attempt=0; $attempt -lt 50; $attempt++) {{ try {{ Move-Item -LiteralPath $From -Destination $To -Force; return }} catch {{ if($attempt -eq 49) {{ throw }}; Start-Sleep -Milliseconds 200 }} }} }}; \
         function Remove-WithRetry([string]$Path) {{ for($attempt=0; $attempt -lt 50; $attempt++) {{ if(-not (Test-Path -LiteralPath $Path)) {{ return }}; try {{ Remove-Item -LiteralPath $Path -Force -ErrorAction Stop; return }} catch {{ if($attempt -eq 49) {{ throw }}; Start-Sleep -Milliseconds 200 }} }} }}; \
         $stages=@('{source_me_s}','{source_gateway}'); $targets=@('{target_me_s}','{target_gateway}'); $backups=@('{backup_me_s}','{backup_gateway}'); $had=@($false,$false); $installed=@($false,$false); \
         try {{ for($i=0; $i -lt 2; $i++) {{ if(Test-Path -LiteralPath $targets[$i]) {{ $item=Get-Item -LiteralPath $targets[$i] -Force; $isReparsePoint=(($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0); if($item.PSIsContainer -or $isReparsePoint) {{ throw \"update target is not a file: $($targets[$i])\" }}; Move-WithRetry $targets[$i] $backups[$i]; $had[$i]=$true }} }}; for($i=0; $i -lt 2; $i++) {{ Move-WithRetry $stages[$i] $targets[$i]; $installed[$i]=$true }} }} catch {{ $message=$_.Exception.Message; $rollbackErrors=@(); for($i=1; $i -ge 0; $i--) {{ if($installed[$i]) {{ try {{ Remove-WithRetry $targets[$i] }} catch {{ $rollbackErrors += \"cannot remove $($targets[$i]): $($_.Exception.Message)\" }} }}; if($had[$i] -and (Test-Path -LiteralPath $backups[$i])) {{ try {{ Move-WithRetry $backups[$i] $targets[$i] }} catch {{ $rollbackErrors += \"cannot restore $($targets[$i]): $($_.Exception.Message)\" }} }} }}; foreach($stage in $stages) {{ Remove-Item -LiteralPath $stage -Force -ErrorAction SilentlyContinue }}; if($rollbackErrors.Count -gt 0) {{ $message += '; rollback also failed: ' + ($rollbackErrors -join '; ') }}; Set-Content -LiteralPath '{error_log}' -Value $message -Encoding UTF8; exit 1 }}; \
         foreach($backup in $backups) {{ Remove-Item -LiteralPath $backup -Force -ErrorAction SilentlyContinue }}; Remove-Item -LiteralPath '{error_log}' -Force -ErrorAction SilentlyContinue; exit 0"
    )
}

#[cfg(windows)]
fn powershell_literal(path: &Path) -> String {
    powershell_literal_text(&path.to_string_lossy())
}

#[cfg(any(windows, test))]
fn powershell_literal_text(path: &str) -> String {
    let normalized = if let Some(path) = path.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{path}")
    } else {
        path.strip_prefix(r"\\?\").unwrap_or(path).to_owned()
    };
    normalized.replace('\'', "''")
}

#[cfg(not(any(unix, windows)))]
fn deploy_product(_downloaded: &[PathBuf; 2], _destinations: &[PathBuf; 2]) -> Result<bool> {
    Err("ME update is not supported on this platform".into())
}

fn sibling_temporary_path(destination: &Path, purpose: &str) -> std::io::Result<PathBuf> {
    let parent = destination.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "current executable has no parent directory",
        )
    })?;
    let file = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("me");
    for _ in 0..16 {
        let suffix = random_suffix().map_err(std::io::Error::other)?;
        let candidate = parent.join(format!(".{file}.{purpose}-{suffix}"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("cannot allocate a unique update {purpose} path"),
    ))
}

fn random_suffix() -> Result<String> {
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random)?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

struct UpdateTempDirectory {
    path: PathBuf,
}

impl UpdateTempDirectory {
    fn create() -> Result<Self> {
        for _ in 0..16 {
            let path = env::temp_dir().join(format!("me-update-{}", random_suffix()?));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!("cannot create update directory: {error}").into());
                }
            }
        }
        Err("cannot allocate a unique update directory".into())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for UpdateTempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::TcpListener, thread};

    fn temporary_directory(name: &str) -> PathBuf {
        let path = env::temp_dir().join(format!(
            "me-updater-test-{name}-{}-{}",
            std::process::id(),
            random_suffix().unwrap()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn release_assets_match_every_supported_target() {
        let cases = [
            (
                "macos",
                "aarch64",
                ["me-s-macos-arm64", "me-gateway-macos-arm64"],
                ["me-s", "me-gateway"],
            ),
            (
                "macos",
                "x86_64",
                ["me-s-macos-x86_64", "me-gateway-macos-x86_64"],
                ["me-s", "me-gateway"],
            ),
            (
                "linux",
                "aarch64",
                ["me-s-linux-arm64", "me-gateway-linux-arm64"],
                ["me-s", "me-gateway"],
            ),
            (
                "linux",
                "x86_64",
                ["me-s-linux-x86_64", "me-gateway-linux-x86_64"],
                ["me-s", "me-gateway"],
            ),
            (
                "windows",
                "x86_64",
                ["me-s-windows-x86_64.exe", "me-gateway-windows-x86_64.exe"],
                ["me-s.exe", "me-gateway.exe"],
            ),
        ];
        for (os, arch, assets, executables) in cases {
            let platform = UpdatePlatform::for_target(os, arch).unwrap();
            assert_eq!(platform.assets(), assets);
            assert_eq!(platform.executable_names(), executables);
        }
        assert!(UpdatePlatform::for_target("windows", "aarch64").is_err());
    }

    #[test]
    fn release_versions_are_numeric_and_never_require_a_downgrade() {
        assert_eq!(release_version("v0.0.164").unwrap(), (0, 0, 164));
        assert_eq!(release_version("1.2.3-beta.1").unwrap(), (1, 2, 3));
        assert!(release_version("v1.2").is_err());
        assert!(release_version("latest").is_err());
        assert!(release_version("v0.0.163").unwrap() < release_version("v0.0.164").unwrap());
    }

    #[test]
    fn public_release_urls_are_versioned_and_need_no_api_metadata() {
        let tagged =
            reqwest::Url::parse("https://github.com/LytsingStudio/me-s/releases/tag/v0.0.267")
                .unwrap();
        assert_eq!(release_tag_from_url(&tagged).unwrap(), "v0.0.267");
        assert!(
            release_tag_from_url(
                &reqwest::Url::parse("https://github.com/LytsingStudio/me-s/releases/latest")
                    .unwrap()
            )
            .is_err()
        );
        assert!(
            release_tag_from_url(
                &reqwest::Url::parse(
                    "https://github.com/LytsingStudio/me-s/releases/tag/not-a-version"
                )
                .unwrap()
            )
            .is_err()
        );

        let release = PublicRelease {
            tag_name: "v0.0.267".into(),
        };
        assert_eq!(
            release_asset_url(&release, "me-s-linux-x86_64")
                .unwrap()
                .as_str(),
            "https://github.com/LytsingStudio/me-s/releases/download/v0.0.267/me-s-linux-x86_64"
        );
    }

    #[test]
    fn latest_release_public_redirect_resolves_without_api_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let read = stream.read(&mut request).unwrap();
            assert!(String::from_utf8_lossy(&request[..read]).starts_with("GET /releases/latest "));
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{address}/releases/tag/v0.0.274\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let read = stream.read(&mut request).unwrap();
            assert!(
                String::from_utf8_lossy(&request[..read])
                    .starts_with("GET /releases/tag/v0.0.274 ")
            );
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
        });
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();

        let release = latest_release_from_sources(
            &client,
            &format!("http://{address}/releases/latest"),
            "http://127.0.0.1:9/api-must-not-be-called",
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(release.tag_name, "v0.0.274");
    }

    #[test]
    fn latest_release_falls_back_to_the_api_after_a_public_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let read = stream.read(&mut request).unwrap();
            assert!(String::from_utf8_lossy(&request[..read]).starts_with("GET /latest "));
            stream
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndown",
                )
                .unwrap();

            let (mut stream, _) = listener.accept().unwrap();
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]);
            assert!(request.starts_with("GET /api/latest "));
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("accept: application/vnd.github+json")
            );
            let body = r#"{"tag_name":"v0.0.307"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let release = latest_release_from_sources(
            &client,
            &format!("http://{address}/latest"),
            &format!("http://{address}/api/latest"),
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(release.tag_name, "v0.0.307");
    }

    #[test]
    fn metadata_checks_are_bounded_separately_from_large_downloads() {
        assert_eq!(METADATA_CONNECT_TIMEOUT, Duration::from_secs(5));
        assert_eq!(METADATA_REQUEST_TIMEOUT, Duration::from_secs(8));
        assert!(DOWNLOAD_CONNECT_TIMEOUT > METADATA_CONNECT_TIMEOUT);
        assert!(DOWNLOAD_REQUEST_TIMEOUT > METADATA_REQUEST_TIMEOUT);
    }

    #[test]
    fn metadata_timeout_returns_a_diagnostic_error_instead_of_hanging() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(250));
        });
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap();
        let started = std::time::Instant::now();
        let error = latest_release_from_url(&client, &format!("http://{address}/latest"))
            .unwrap_err()
            .to_string();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(error.contains("public release redirect"), "{error}");
        assert!(error.to_ascii_lowercase().contains("timed out"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn update_temporary_directory_removes_partial_downloads_on_drop() {
        let path = {
            let temporary = UpdateTempDirectory::create().unwrap();
            let path = temporary.path().to_owned();
            fs::write(path.join("partial-download"), b"partial").unwrap();
            assert!(path.exists());
            path
        };

        assert!(!path.exists());
    }

    #[test]
    fn release_asset_download_streams_directly_to_the_target_file() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let read = stream.read(&mut request).unwrap();
            assert!(String::from_utf8_lossy(&request[..read]).starts_with("GET /asset "));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 14\r\nConnection: close\r\n\r\nrelease-bytes!",
                )
                .unwrap();
        });
        let directory = temporary_directory("download");
        let destination = directory.join("asset");
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        download_asset(
            &client,
            "asset",
            &format!("http://{address}/asset"),
            &destination,
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"release-bytes!");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn windows_helper_normalizes_verbatim_paths_and_quotes_literals() {
        assert_eq!(
            WINDOWS_UPDATE_POWERSHELL_ARGS,
            ["-NoLogo", "-NoProfile", "-NonInteractive"]
        );
        assert!(
            WINDOWS_UPDATE_POWERSHELL_ARGS
                .iter()
                .all(|argument| !argument.eq_ignore_ascii_case("-WindowStyle"))
        );
        assert_eq!(
            powershell_literal_text(r"\\?\C:\Users\O'Brien\me-s.exe"),
            r"C:\Users\O''Brien\me-s.exe"
        );
        assert_eq!(
            powershell_literal_text(r"\\?\UNC\server\share\me-s.exe"),
            r"\\server\share\me-s.exe"
        );
        let script = windows_update_script(
            42,
            "C:\\staged-me-s.exe",
            "C:\\me-s.exe",
            "C:\\backup-me-s.exe",
            "C:\\staged-me-gateway.exe",
            "C:\\me-gateway.exe",
            "C:\\backup-me-gateway.exe",
            "C:\\me-update-error.log",
        );
        assert!(script.contains("Wait-Process -Id 42"));
        assert!(script.contains("C:\\staged-me-s.exe"));
        assert!(script.contains("C:\\staged-me-gateway.exe"));
        assert!(script.contains("C:\\me-s.exe"));
        assert!(script.contains("C:\\me-gateway.exe"));
        assert!(script.contains("$installed=@($false,$false)"));
        assert!(script.contains("Set-Content -LiteralPath 'C:\\me-update-error.log'"));
        assert!(script.contains("for($i=1; $i -ge 0; $i--)"));
        assert!(script.contains("function Remove-WithRetry"));
        assert!(script.contains("$rollbackErrors=@()"));
        assert!(script.contains("rollback also failed"));
        assert!(!script.contains("WindowStyle"));
    }

    #[test]
    fn checksum_manifest_requires_one_exact_valid_entry() {
        let digest = "a".repeat(64);
        let manifest = format!(
            "{}  me-s-linux-x86_64\n{} *me-s-windows-x86_64.exe\n",
            digest,
            "B".repeat(64)
        );
        assert_eq!(
            checksum_for_asset(&manifest, "me-s-linux-x86_64").unwrap(),
            digest
        );
        assert_eq!(
            checksum_for_asset(&manifest, "me-s-windows-x86_64.exe").unwrap(),
            "b".repeat(64)
        );
        assert!(checksum_for_asset(&manifest, "me").is_err());
        assert!(checksum_for_asset("bad  me-s-linux-x86_64", "me-s-linux-x86_64").is_err());
        let duplicate = format!("{digest}  me-s\n{digest}  me-s\n");
        assert!(checksum_for_asset(&duplicate, "me-s").is_err());
        assert!(checksum_for_asset(&format!("{digest}  me-s extra-field\n"), "me-s").is_err());
        assert!(checksum_for_asset(&format!("{digest}  **me-s\n"), "me-s").is_err());
    }

    #[test]
    fn release_verification_hashes_the_exact_downloaded_bytes() {
        let directory = temporary_directory("checksum");
        let asset = directory.join("me-s-linux-x86_64");
        let manifest = directory.join(CHECKSUM_ASSET);
        fs::write(&asset, b"abc").unwrap();
        fs::write(
            &manifest,
            b"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  me-s-linux-x86_64\n",
        )
        .unwrap();

        verify_release_asset(&asset, &manifest, "me-s-linux-x86_64").unwrap();
        fs::write(&asset, b"changed").unwrap();
        assert!(verify_release_asset(&asset, &manifest, "me-s-linux-x86_64").is_err());
        fs::write(&asset, b"").unwrap();
        assert!(verify_release_asset(&asset, &manifest, "me-s-linux-x86_64").is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn same_version_is_complete_only_when_both_programs_report_it() {
        use std::os::unix::fs::PermissionsExt;

        let directory = temporary_directory("complete-product");
        let destinations = [directory.join("me-s"), directory.join("me-gateway")];
        let write_program = |path: &Path, name: &str, version: &str| {
            fs::write(
                path,
                format!("#!/bin/sh\nprintf '%s\\n' '{name} {version}'\n"),
            )
            .unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        };
        write_program(&destinations[0], "me-s", "1.2.3");
        assert!(!installed_product_matches(&destinations, "1.2.3"));
        write_program(&destinations[1], "me-gateway", "1.2.2");
        assert!(!installed_product_matches(&destinations, "1.2.3"));
        write_program(&destinations[1], "me-gateway", "1.2.3");
        assert!(installed_product_matches(&destinations, "1.2.3"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn downloaded_product_must_execute_and_report_the_exact_release_version() {
        let directory = temporary_directory("downloaded-product");
        let programs = [directory.join("me-s"), directory.join("me-gateway")];
        fs::write(&programs[0], b"#!/bin/sh\nprintf 'me-s 1.2.3\\n'\n").unwrap();
        fs::write(&programs[1], b"#!/bin/sh\nprintf 'me-gateway 1.2.3\\n'\n").unwrap();
        validate_downloaded_product(&programs, "1.2.3").unwrap();
        fs::write(&programs[1], b"#!/bin/sh\nprintf 'me-gateway 1.2.2\\n'\n").unwrap();
        assert!(validate_downloaded_product(&programs, "1.2.3").is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unix_install_updates_both_programs_and_preserves_unrelated_files() {
        use std::os::unix::fs::PermissionsExt;

        let directory = temporary_directory("install");
        let downloaded = [
            directory.join("downloaded-me-s"),
            directory.join("downloaded-me-gateway"),
        ];
        let destinations = [directory.join("me-s"), directory.join("me-gateway")];
        let original_me = directory.join("me");
        let configuration = directory.join("models.toml");
        fs::write(&downloaded[0], b"new me-s executable").unwrap();
        fs::write(&downloaded[1], b"new me-gateway executable").unwrap();
        fs::write(&destinations[0], b"old me-s executable").unwrap();
        fs::write(&destinations[1], b"old me-gateway executable").unwrap();
        fs::write(&original_me, b"original me executable").unwrap();
        fs::write(&configuration, b"keep configuration").unwrap();

        atomic_install_product_unix(&downloaded, &destinations).unwrap();

        assert_eq!(fs::read(&destinations[0]).unwrap(), b"new me-s executable");
        assert_eq!(
            fs::read(&destinations[1]).unwrap(),
            b"new me-gateway executable"
        );
        assert_eq!(fs::read(&original_me).unwrap(), b"original me executable");
        assert_eq!(fs::read(&configuration).unwrap(), b"keep configuration");
        for destination in &destinations {
            assert_eq!(
                fs::metadata(destination).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
        assert!(fs::read_dir(&directory).unwrap().all(|entry| {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            !name.contains(".update-") && !name.contains(".backup-")
        }));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unix_install_rolls_back_both_programs_when_second_commit_fails() {
        let directory = temporary_directory("rollback");
        let destinations = [directory.join("me-s"), directory.join("me-gateway")];
        let staging = [
            directory.join("staged-me-s"),
            directory.join("missing-staged-me-gateway"),
        ];
        let backups = [
            directory.join("backup-me-s"),
            directory.join("backup-me-gateway"),
        ];
        fs::write(&destinations[0], b"old me-s").unwrap();
        fs::write(&destinations[1], b"old gateway").unwrap();
        fs::write(&staging[0], b"new me-s").unwrap();

        assert!(commit_staged_product_unix(&staging, &destinations, &backups).is_err());
        assert_eq!(fs::read(&destinations[0]).unwrap(), b"old me-s");
        assert_eq!(fs::read(&destinations[1]).unwrap(), b"old gateway");
        assert!(!backups[0].exists());
        assert!(!backups[1].exists());
        assert!(!staging[0].exists());
        fs::remove_dir_all(directory).unwrap();
    }
}
