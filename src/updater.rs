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
enum InstallerKind {
    MacosPkg,
    LinuxRun,
    WindowsNsis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UpdatePlatform {
    package_asset: &'static str,
    installer: InstallerKind,
    me_s_executable: &'static str,
    gateway_executable: &'static str,
    client_executable: &'static str,
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
        let platform = match (os, arch) {
            ("macos", "aarch64" | "x86_64") => Self {
                package_asset: "ME-macos-universal.pkg",
                installer: InstallerKind::MacosPkg,
                me_s_executable: "me-s",
                gateway_executable: "me-gateway",
                client_executable: "me-client",
            },
            ("linux", "aarch64") => Self {
                package_asset: "ME-linux-arm64.run",
                installer: InstallerKind::LinuxRun,
                me_s_executable: "me-s",
                gateway_executable: "me-gateway",
                client_executable: "me-client",
            },
            ("linux", "x86_64") => Self {
                package_asset: "ME-linux-x86_64.run",
                installer: InstallerKind::LinuxRun,
                me_s_executable: "me-s",
                gateway_executable: "me-gateway",
                client_executable: "me-client",
            },
            ("windows", "x86_64") => Self {
                package_asset: "ME-windows-x86_64-setup.exe",
                installer: InstallerKind::WindowsNsis,
                me_s_executable: "me-s.exe",
                gateway_executable: "me-gateway.exe",
                client_executable: "me-client.exe",
            },
            _ => return Err(format!("ME update does not support {os}/{arch}").into()),
        };
        Ok(platform)
    }

    fn cli_destinations(self, install_directory: &Path) -> [PathBuf; 2] {
        [
            install_directory.join(self.me_s_executable),
            install_directory.join(self.gateway_executable),
        ]
    }

    fn client_destination(self, install_directory: &Path) -> PathBuf {
        if self.installer == InstallerKind::MacosPkg {
            PathBuf::from("/Applications/ME Client.app/Contents/MacOS/me-client")
        } else {
            install_directory.join(self.client_executable)
        }
    }
}

pub fn update() -> Result<()> {
    let platform = UpdatePlatform::detect()?;
    let running = env::current_exe()
        .map_err(|error| format!("cannot locate the running ME executable: {error}"))?;
    let install_directory = running
        .parent()
        .ok_or("the running ME executable has no installation directory")?;

    let metadata_client = update_metadata_client()?;
    let release = latest_release(&metadata_client)?;
    let latest_tag = release.tag_name.as_str();
    let current_tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    if latest_tag == current_tag
        && installed_product_matches(install_directory, platform, env!("CARGO_PKG_VERSION"))
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
    download_release_package(&download_client, &release, platform, temporary.path())?;
    let package = temporary.path().join(platform.package_asset);
    let checksums = temporary.path().join(CHECKSUM_ASSET);
    verify_release_asset(&package, &checksums, platform.package_asset)?;
    let expected_version = latest_tag.strip_prefix('v').unwrap_or(latest_tag);
    validate_downloaded_package(&package, platform, expected_version)?;

    let scheduled_after_exit = deploy_product_package(&package, install_directory, platform)?;
    if scheduled_after_exit {
        let error_log = install_directory.join(".me-update-error.log");
        println!(
            "downloaded and verified ME {latest_tag}; Windows will install the complete product after this process exits\nerror log if installation cannot be completed: {}\nglobal configuration: unchanged",
            error_log.display()
        );
    } else {
        if !installed_product_matches(install_directory, platform, expected_version) {
            return Err(
                "the ME product installer completed but the installed product is incomplete".into(),
            );
        }
        println!(
            "updated ME to {latest_tag}\nme-s: {}\nme-gateway: {}\nme-client: {}\nglobal configuration: unchanged",
            install_directory.join(platform.me_s_executable).display(),
            install_directory
                .join(platform.gateway_executable)
                .display(),
            platform.client_destination(install_directory).display()
        );
    }
    Ok(())
}

fn installed_product_matches(
    install_directory: &Path,
    platform: UpdatePlatform,
    version: &str,
) -> bool {
    let destinations = platform.cli_destinations(install_directory);
    for (destination, program) in destinations.iter().zip(["me-s", "me-gateway"]) {
        let Ok(metadata) = fs::symlink_metadata(destination) else {
            return false;
        };
        if !metadata.file_type().is_file() || metadata.len() == 0 {
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
    fs::metadata(platform.client_destination(install_directory))
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0)
}

fn validate_downloaded_package(
    package: &Path,
    platform: UpdatePlatform,
    _version: &str,
) -> Result<()> {
    if !package.is_file() || package.metadata()?.len() == 0 {
        return Err(format!("downloaded product package is empty: {}", package.display()).into());
    }
    if platform.installer == InstallerKind::LinuxRun {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(package, fs::Permissions::from_mode(0o755))?;
            let output = Command::new(package)
                .arg("--version")
                .output()
                .map_err(|error| {
                    format!("downloaded Linux product package could not be executed: {error}")
                })?;
            let expected = format!("ME product {_version}");
            let actual = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if !output.status.success() || actual != expected {
                return Err(format!(
                    "downloaded Linux product package reported an unexpected version: expected {expected}, received {actual}"
                )
                .into());
            }
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
            "cannot query the latest public ME release; public redirect failed: {public_error}; GitHub API fallback failed: {api_error}"
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
            "cannot query the latest public ME release: HTTP {status}{}",
            response_detail(&detail)
        )
        .into());
    }
    Ok(PublicRelease {
        tag_name: release_tag_from_url(response.url())?,
    })
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

fn download_release_package(
    client: &reqwest::blocking::Client,
    release: &PublicRelease,
    platform: UpdatePlatform,
    directory: &Path,
) -> Result<()> {
    for name in [platform.package_asset, CHECKSUM_ASSET] {
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
        .map_err(|_| format!("invalid ME release tag: {tag}"))?;
    let [major, minor, patch] = values.as_slice() else {
        return Err(format!("invalid ME release tag: {tag}").into());
    };
    Ok((*major, *minor, *patch))
}

fn verify_release_asset(path: &Path, manifest: &Path, asset: &str) -> Result<()> {
    if !path.is_file() || path.metadata()?.len() == 0 {
        return Err(format!("release asset was not downloaded: {asset}").into());
    }
    let manifest = fs::read_to_string(manifest)
        .map_err(|error| format!("cannot read downloaded {CHECKSUM_ASSET}: {error}"))?;
    let expected = checksum_for_asset(&manifest, asset)?;
    let actual = sha256_file(path)?;
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
        if fields.next().is_some() || found.is_some() {
            return Err(format!("{CHECKSUM_ASSET} contains an invalid entry for {asset}").into());
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

fn deploy_product_package(
    package: &Path,
    install_directory: &Path,
    platform: UpdatePlatform,
) -> Result<bool> {
    #[cfg(target_os = "macos")]
    if platform.installer == InstallerKind::MacosPkg {
        if install_directory != Path::new("/usr/local/bin") {
            return Err(
                "the macOS ME product package requires the standard /usr/local/bin installation"
                    .into(),
            );
        }
        run_checked(
            Command::new("sudo")
                .arg("installer")
                .arg("-pkg")
                .arg(package)
                .arg("-target")
                .arg("/"),
            "install the macOS ME product package",
        )?;
        return Ok(false);
    }

    #[cfg(target_os = "linux")]
    if platform.installer == InstallerKind::LinuxRun {
        run_checked(
            Command::new(package)
                .arg("--install-dir")
                .arg(install_directory),
            "install the Linux ME product package",
        )?;
        return Ok(false);
    }

    #[cfg(windows)]
    if platform.installer == InstallerKind::WindowsNsis {
        return schedule_windows_installer(package, install_directory);
    }

    Err("ME product installer does not match the current platform".into())
}

#[cfg(any(unix, test))]
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
fn schedule_windows_installer(package: &Path, install_directory: &Path) -> Result<bool> {
    let staged = sibling_temporary_path(
        &install_directory.join("ME-windows-x86_64-setup.exe"),
        "update",
    )?;
    fs::copy(package, &staged).map_err(|error| {
        format!(
            "cannot stage the Windows product installer beside {}: {error}",
            install_directory.display()
        )
    })?;
    let error_log = install_directory.join(".me-update-error.log");
    let _ = fs::remove_file(&error_log);
    let script = windows_update_script(
        std::process::id(),
        &powershell_literal(&staged),
        &powershell_literal(&error_log),
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
        let _ = fs::remove_file(&staged);
        return Err(format!("cannot start the Windows product update helper: {error}").into());
    }
    Ok(true)
}

#[cfg(any(windows, test))]
fn windows_update_script(process_id: u32, setup: &str, error_log: &str) -> String {
    format!(
        "$ErrorActionPreference='Stop'; Wait-Process -Id {process_id} -ErrorAction SilentlyContinue; try {{ $process=Start-Process -FilePath '{setup}' -ArgumentList '/S' -Wait -PassThru; if($process.ExitCode -ne 0) {{ throw \"ME installer exited with code $($process.ExitCode)\" }}; Remove-Item -LiteralPath '{setup}' -Force -ErrorAction SilentlyContinue; Remove-Item -LiteralPath '{error_log}' -Force -ErrorAction SilentlyContinue }} catch {{ Set-Content -LiteralPath '{error_log}' -Value $_.Exception.Message -Encoding UTF8; exit 1 }}"
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

#[cfg(windows)]
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
                Err(error) => return Err(format!("cannot create update directory: {error}").into()),
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
    fn release_packages_match_supported_targets() {
        let cases = [
            (
                "macos",
                "aarch64",
                "ME-macos-universal.pkg",
                InstallerKind::MacosPkg,
            ),
            (
                "macos",
                "x86_64",
                "ME-macos-universal.pkg",
                InstallerKind::MacosPkg,
            ),
            (
                "linux",
                "aarch64",
                "ME-linux-arm64.run",
                InstallerKind::LinuxRun,
            ),
            (
                "linux",
                "x86_64",
                "ME-linux-x86_64.run",
                InstallerKind::LinuxRun,
            ),
            (
                "windows",
                "x86_64",
                "ME-windows-x86_64-setup.exe",
                InstallerKind::WindowsNsis,
            ),
        ];
        for (os, arch, asset, installer) in cases {
            let platform = UpdatePlatform::for_target(os, arch).unwrap();
            assert_eq!(platform.package_asset, asset);
            assert_eq!(platform.installer, installer);
        }
        assert!(UpdatePlatform::for_target("windows", "aarch64").is_err());
    }

    #[test]
    fn release_versions_and_urls_are_versioned() {
        assert_eq!(release_version("v1.2.3").unwrap(), (1, 2, 3));
        assert!(release_version("latest").is_err());
        let release = PublicRelease {
            tag_name: "v1.2.3".into(),
        };
        assert_eq!(
            release_asset_url(&release, "ME-linux-x86_64.run")
                .unwrap()
                .as_str(),
            "https://github.com/LytsingStudio/me-s/releases/download/v1.2.3/ME-linux-x86_64.run"
        );
    }

    #[test]
    fn checksum_manifest_requires_one_exact_entry() {
        let digest = "a".repeat(64);
        let manifest = format!("{digest}  ME-linux-x86_64.run\n");
        assert_eq!(
            checksum_for_asset(&manifest, "ME-linux-x86_64.run").unwrap(),
            digest
        );
        assert!(checksum_for_asset(&manifest, "ME-linux-arm64.run").is_err());
        let duplicate = format!("{digest}  ME-linux-x86_64.run\n{digest}  ME-linux-x86_64.run\n");
        assert!(checksum_for_asset(&duplicate, "ME-linux-x86_64.run").is_err());
    }

    #[test]
    fn public_release_redirect_resolves_without_api_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let read = stream.read(&mut request).unwrap();
            assert!(String::from_utf8_lossy(&request[..read]).starts_with("GET /latest "));
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{address}/releases/tag/v1.2.3\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let read = stream.read(&mut request).unwrap();
            assert!(
                String::from_utf8_lossy(&request[..read]).starts_with("GET /releases/tag/v1.2.3 ")
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
            &format!("http://{address}/latest"),
            "http://127.0.0.1:9/api-unused",
        )
        .unwrap();
        server.join().unwrap();
        assert_eq!(release.tag_name, "v1.2.3");
    }

    #[test]
    fn downloaded_asset_is_streamed_and_verified() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc")
                .unwrap();
        });
        let directory = temporary_directory("download");
        let asset = directory.join("ME-linux-x86_64.run");
        let manifest = directory.join(CHECKSUM_ASSET);
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        download_asset(
            &client,
            "ME-linux-x86_64.run",
            &format!("http://{address}/asset"),
            &asset,
        )
        .unwrap();
        server.join().unwrap();
        fs::write(&manifest, b"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  ME-linux-x86_64.run\n").unwrap();
        verify_release_asset(&asset, &manifest, "ME-linux-x86_64.run").unwrap();
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn complete_product_requires_both_clis_and_client() {
        use std::os::unix::fs::PermissionsExt;
        let directory = temporary_directory("complete");
        for (name, identity) in [("me-s", "me-s"), ("me-gateway", "me-gateway")] {
            let path = directory.join(name);
            fs::write(&path, format!("#!/bin/sh\nprintf '{identity} 1.2.3\\n'\n")).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let client = directory.join("me-client");
        fs::write(&client, b"client").unwrap();
        let platform = UpdatePlatform::for_target("linux", "x86_64").unwrap();
        assert!(installed_product_matches(&directory, platform, "1.2.3"));
        fs::remove_file(client).unwrap();
        assert!(!installed_product_matches(&directory, platform, "1.2.3"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn windows_helper_waits_and_runs_the_complete_installer() {
        assert_eq!(
            WINDOWS_UPDATE_POWERSHELL_ARGS,
            ["-NoLogo", "-NoProfile", "-NonInteractive"]
        );
        let script = windows_update_script(42, "C:\\ME setup.exe", "C:\\error.log");
        assert!(script.contains("Wait-Process -Id 42"));
        assert!(script.contains("Start-Process"));
        assert!(script.contains("-ArgumentList '/S'"));
        assert!(script.contains("C:\\ME setup.exe"));
        assert!(script.contains("Set-Content -LiteralPath 'C:\\error.log'"));
        assert_eq!(
            powershell_literal_text(r"\\?\C:\ME O'Brien\setup.exe"),
            r"C:\ME O''Brien\setup.exe"
        );
    }

    #[test]
    fn update_temporary_directory_cleans_partial_downloads() {
        let path = {
            let temporary = UpdateTempDirectory::create().unwrap();
            let path = temporary.path().to_owned();
            fs::write(path.join("partial"), b"partial").unwrap();
            path
        };
        assert!(!path.exists());
    }
}
