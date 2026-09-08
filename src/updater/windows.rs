use std::{
    env, fs,
    fs::{File, OpenOptions},
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::{
    Foundation::{ERROR_INVALID_PARAMETER, FILETIME, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0},
    Storage::FileSystem::FILE_SHARE_DELETE,
    System::Threading::{
        CREATE_NO_WINDOW, CreateMutexW, GetCurrentProcess, GetProcessTimes, OpenProcess,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, ReleaseMutex, WaitForSingleObject,
    },
};

use super::{
    portable::{self, PROGRAMS},
    random_suffix, sha256_file,
};
use crate::Result;

const HELPER_ARGUMENT: &str = "--me-update-helper";
const CLEANUP_ARGUMENT: &str = "--me-update-cleanup";
const STAGE_PREFIX: &str = ".me-update-";
const ERROR_LOG: &str = ".me-update-error.log";

#[derive(Serialize, Deserialize)]
struct UpdateJob {
    parent_id: u32,
    parent_started: u64,
    version: String,
    digests: Vec<String>,
}

fn process_started(handle: HANDLE) -> Result<u64> {
    let mut created: FILETIME = unsafe { std::mem::zeroed() };
    let mut exited = created;
    let mut kernel = created;
    let mut user = created;
    if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

fn wait_for_process(id: u32, started: u64) -> Result<()> {
    let raw = unsafe {
        OpenProcess(
            PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            id,
        )
    };
    if raw.is_null() {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
            return Ok(());
        }
        return Err(error.into());
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    // A reused PID is not the process that initiated this update.
    if process_started(handle.as_raw_handle())? != started {
        return Ok(());
    }
    if unsafe { WaitForSingleObject(handle.as_raw_handle(), 120_000) } != WAIT_OBJECT_0 {
        return Err("the initiating update process did not exit within two minutes".into());
    }
    Ok(())
}

struct UpdateLock(OwnedHandle);
impl UpdateLock {
    fn acquire(directory: &Path) -> Result<Self> {
        use sha2::{Digest, Sha256};
        let identity = fs::canonicalize(directory)?
            .to_string_lossy()
            .to_lowercase();
        let name = format!("Local\\ME-Update-{:x}", Sha256::digest(identity.as_bytes()));
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let raw = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
        if raw.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Self(handle)),
            _ => Err("another ME update is already replacing this installation".into()),
        }
    }
}
impl Drop for UpdateLock {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0.as_raw_handle());
        }
    }
}

fn hidden_command(program: &Path) -> Command {
    let mut command = Command::new(program);
    command
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null());
    if let Some(directory) = program.parent() {
        command.current_dir(directory);
    }
    command
}

pub(super) fn verify_versions(directory: &Path, version: &str) -> Result<()> {
    for name in PROGRAMS {
        let metadata = fs::symlink_metadata(directory.join(name))?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(format!("missing or invalid program: {name}").into());
        }
        let mut child = hidden_command(&directory.join(name))
            .arg("version")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.try_wait()?.is_none() {
            if Instant::now() >= deadline {
                // This is only the short-lived version probe created above, never a user's running component.
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("version verification timed out for {name}").into());
            }
            thread::sleep(Duration::from_millis(20));
        }
        let output = child.wait_with_output()?;
        let expected = format!("{} {version}", name.trim_end_matches(".exe"));
        if !output.status.success() || String::from_utf8_lossy(&output.stdout).trim() != expected {
            return Err(
                format!("unexpected version reported by {name}; expected {expected}").into(),
            );
        }
    }
    Ok(())
}

fn program_digests(directory: &Path) -> Result<Vec<String>> {
    PROGRAMS
        .iter()
        .map(|name| sha256_file(&directory.join(name)))
        .collect()
}

fn lock_installed_programs(directory: &Path) -> Result<Vec<File>> {
    let mut files = Vec::new();
    for name in PROGRAMS {
        let path = directory.join(name);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
            Ok(metadata) if !metadata.is_file() => {
                return Err(format!("not a regular program: {}", path.display()).into());
            }
            Ok(_) => {}
        }
        // Deny new readers/writers while allowing our backup rename of the old file.
        // A mapped/running executable cannot be opened for writing: fail rather than terminating it.
        files.push(
            OpenOptions::new()
                .read(true)
                .write(true)
                .share_mode(FILE_SHARE_DELETE)
                .open(&path)
                .map_err(|error| {
                    format!(
                        "cannot replace {name}; close all ME programs in {} and retry: {error}",
                        directory.display()
                    )
                })?,
        );
    }
    Ok(files)
}

pub(super) fn schedule(package: &Path, install: &Path, version: &str) -> Result<bool> {
    let install = fs::canonicalize(install)?;
    let stage = loop {
        let candidate = install.join(format!("{STAGE_PREFIX}{}", random_suffix()?));
        match fs::create_dir(&candidate) {
            Ok(()) => break candidate,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(
                    format!("cannot prepare an update in {}: {error}", install.display()).into(),
                );
            }
        }
    };
    let result = (|| -> Result<()> {
        let payload = stage.join("new");
        portable::extract(package, &payload)?;
        verify_versions(&payload, version)?;
        let job = UpdateJob {
            parent_id: std::process::id(),
            parent_started: process_started(unsafe { GetCurrentProcess() })?,
            version: version.to_owned(),
            digests: program_digests(&payload)?,
        };
        fs::write(stage.join("job.json"), serde_json::to_vec(&job)?)?;
        let helper = stage.join("helper.exe");
        fs::copy(env::current_exe()?, &helper)?;
        hidden_command(&helper)
            .arg(HELPER_ARGUMENT)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&stage);
        return Err(error);
    }
    Ok(true)
}

fn stage_name_valid(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix(STAGE_PREFIX))
        .is_some_and(|suffix| {
            suffix.len() == 16 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

fn execute_update(stage: &Path) -> Result<()> {
    let install = stage.parent().ok_or("missing installation directory")?;
    let job: UpdateJob = serde_json::from_slice(&fs::read(stage.join("job.json"))?)?;
    super::release_version(&job.version)?;
    wait_for_process(job.parent_id, job.parent_started)?;
    let _lock = UpdateLock::acquire(install)?;
    let payload = stage.join("new");
    if program_digests(&payload)? != job.digests {
        return Err("staged portable programs changed before installation".into());
    }
    verify_versions(&payload, &job.version)?;
    let guards = lock_installed_programs(install)?;
    portable::replace_product(install, &payload, &stage.join("backup"), |installed| {
        if program_digests(installed)? != job.digests {
            return Err("installed program verification failed".into());
        }
        verify_versions(installed, &job.version)
    })?;
    drop(guards);
    let log = install.join(ERROR_LOG);
    if fs::symlink_metadata(&log).is_ok_and(|metadata| metadata.is_file()) {
        let _ = fs::remove_file(log);
    }
    // The helper is itself locked while running. A special, service-free invocation of
    // the newly installed CLI removes only this staging directory after the helper exits.
    let helper_id = std::process::id();
    let helper_started = process_started(unsafe { GetCurrentProcess() })?;
    fs::write(
        stage.join("completed.json"),
        serde_json::to_vec(&(helper_id, helper_started))?,
    )?;
    hidden_command(&install.join("me-s.exe"))
        .arg(CLEANUP_ARGUMENT)
        .arg(stage)
        .arg(helper_id.to_string())
        .arg(helper_started.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            format!("ME was updated, but temporary cleanup could not start: {error}")
        })?;
    Ok(())
}

pub(super) fn run_helper_if_requested() -> Result<bool> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    let Some(argument) = args.first() else {
        return Ok(false);
    };
    if argument == HELPER_ARGUMENT {
        if args.len() != 1 {
            return Err("invalid update helper arguments".into());
        }
        let executable = fs::canonicalize(env::current_exe()?)?;
        let stage = executable
            .parent()
            .ok_or("missing update helper directory")?;
        if executable.file_name().and_then(|name| name.to_str()) != Some("helper.exe")
            || !stage_name_valid(stage)
        {
            return Err("the update helper must run from its own staging directory".into());
        }
        if let Err(error) = execute_update(stage) {
            let detail = format!("{error}\nUpdate files retained at: {}\n", stage.display());
            let _ = fs::write(stage.join("error.log"), &detail);
            let log = stage
                .parent()
                .ok_or("missing installation directory")?
                .join(ERROR_LOG);
            match fs::symlink_metadata(&log) {
                Ok(metadata) if metadata.is_file() => {
                    fs::write(log, detail)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::write(log, detail)?;
                }
                _ => {}
            }
            return Err(error);
        }
        return Ok(true);
    }
    if argument == CLEANUP_ARGUMENT {
        if args.len() != 4 {
            return Err("invalid update cleanup arguments".into());
        }
        let stage = fs::canonicalize(PathBuf::from(&args[1]))?;
        let executable = fs::canonicalize(env::current_exe()?)?;
        if stage.parent() != executable.parent() || !stage_name_valid(&stage) {
            return Err(
                "update cleanup is restricted to this installation's staging directory".into(),
            );
        }
        let id = args[2].to_str().ok_or("invalid helper PID")?.parse()?;
        let started = args[3]
            .to_str()
            .ok_or("invalid helper start time")?
            .parse()?;
        let completed: (u32, u64) =
            serde_json::from_slice(&fs::read(stage.join("completed.json"))?)?;
        if completed != (id, started) {
            return Err("update cleanup does not match a completed update".into());
        }
        wait_for_process(id, started)?;
        fs::remove_dir_all(stage)?;
        return Ok(true);
    }
    Ok(false)
}
