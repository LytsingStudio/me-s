use crate::Result;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use flate2::{Compression, write::GzEncoder};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
use tar::Builder;

pub(crate) const MAX_UPLOAD_CHUNK_BYTES: usize = 512 * 1024;
const COPY_BUFFER_BYTES: usize = 128 * 1024;

#[derive(Clone)]
pub(crate) struct HostFileManager {
    workspace: PathBuf,
    jobs: Arc<Mutex<HashMap<String, Arc<Mutex<JobRecord>>>>>,
    uploads: Arc<Mutex<HashMap<String, UploadSession>>>,
    downloads: Arc<Mutex<HashMap<String, Arc<Mutex<DownloadRecord>>>>>,
    next_id: Arc<AtomicU64>,
    filesystem_lock: Arc<Mutex<()>>,
    shutdown: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct HostDirectoryListing {
    pub ok: bool,
    pub path: Option<String>,
    pub parent: Option<String>,
    pub root_selector: bool,
    pub parent_is_root_selector: bool,
    pub entries: Vec<HostDirectoryEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct HostDirectoryEntry {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub size_bytes: Option<u64>,
    pub modified_at_ms: Option<u64>,
    pub hidden: bool,
    pub readonly: bool,
    pub symlink: bool,
    pub navigable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HostFileJobKind {
    Copy,
    Move,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HostFileConflictPolicy {
    Replace,
    Skip,
    KeepBoth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HostFileJobState {
    Planning,
    AwaitingConfirmation,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct HostFileStats {
    pub items: u64,
    pub files: u64,
    pub directories: u64,
    pub symlinks: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct HostFileConflict {
    pub source: String,
    pub target: String,
    pub target_kind: String,
    pub directory_replacement: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct HostFileItemResult {
    pub source: String,
    pub target: Option<String>,
    pub status: String,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct HostFileJobSnapshot {
    pub ok: bool,
    pub operation_id: String,
    pub kind: HostFileJobKind,
    pub state: HostFileJobState,
    pub sources: Vec<String>,
    pub destination: Option<String>,
    pub stats: HostFileStats,
    pub conflicts: Vec<HostFileConflict>,
    pub processed_items: u64,
    pub processed_bytes: u64,
    pub current_path: Option<String>,
    pub succeeded: u64,
    pub skipped: u64,
    pub failed: u64,
    pub results: Vec<HostFileItemResult>,
    pub error: Option<String>,
    pub created_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub cancellable: bool,
}

struct JobRecord {
    id: String,
    kind: HostFileJobKind,
    state: HostFileJobState,
    sources: Vec<PathBuf>,
    source_stats: Vec<HostFileStats>,
    destination: Option<PathBuf>,
    stats: HostFileStats,
    conflicts: Vec<HostFileConflict>,
    processed_items: u64,
    processed_bytes: u64,
    current_path: Option<PathBuf>,
    succeeded: u64,
    skipped: u64,
    failed: u64,
    results: Vec<HostFileItemResult>,
    error: Option<String>,
    created_at_ms: u64,
    finished_at_ms: Option<u64>,
    cancel: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UploadCreateResult {
    pub ok: bool,
    pub requires_confirmation: bool,
    pub conflict: Option<HostFileConflict>,
    pub upload: Option<UploadSnapshot>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UploadSnapshot {
    pub ok: bool,
    pub upload_id: String,
    pub state: String,
    pub target_path: String,
    pub size_bytes: u64,
    pub received_bytes: u64,
    pub error: Option<String>,
}

struct UploadSession {
    id: String,
    temp_path: PathBuf,
    target_path: PathBuf,
    size_bytes: u64,
    received_bytes: u64,
    file: File,
    policy: Option<HostFileConflictPolicy>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DownloadSnapshot {
    pub ok: bool,
    pub download_id: String,
    pub state: String,
    pub filename: String,
    pub size_bytes: Option<u64>,
    pub error: Option<String>,
    pub created_at_ms: u64,
}

struct DownloadRecord {
    id: String,
    state: String,
    filename: String,
    path: Option<PathBuf>,
    owned_temp: bool,
    size_bytes: Option<u64>,
    error: Option<String>,
    created_at_ms: u64,
    cancel: Arc<AtomicBool>,
}

pub(crate) struct DownloadStream {
    pub reader: Box<dyn Read + Send>,
    pub status: u16,
    pub content_length: usize,
    pub content_range: Option<String>,
    pub filename: String,
    pub content_type: &'static str,
}

enum CommitOutcome {
    Complete,
    Partial(String),
}

impl HostFileManager {
    pub(crate) fn new(workspace: impl AsRef<Path>) -> Result<Self> {
        let workspace = fs::canonicalize(workspace.as_ref())?;
        if !workspace.is_dir() {
            return Err("Workspace path is not a directory".into());
        }
        Ok(Self {
            workspace,
            jobs: Arc::new(Mutex::new(HashMap::new())),
            uploads: Arc::new(Mutex::new(HashMap::new())),
            downloads: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            filesystem_lock: Arc::new(Mutex::new(())),
            shutdown: Arc::new(AtomicBool::new(false)),
        })
    }

    pub(crate) fn list(&self, path: Option<&str>, roots: bool) -> Result<HostDirectoryListing> {
        if roots {
            return Ok(HostDirectoryListing {
                ok: true,
                path: None,
                parent: None,
                root_selector: true,
                parent_is_root_selector: false,
                entries: host_roots()?,
            });
        }
        let directory = canonical_directory(
            path.map(PathBuf::from)
                .unwrap_or_else(|| self.workspace.clone()),
        )?;
        let mut entries = Vec::new();
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(is_internal_temp_name)
            {
                continue;
            }
            entries.push(directory_entry(&entry.path())?);
        }
        entries.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.name.cmp(&right.name))
        });
        let parent = directory.parent().map(path_string);
        Ok(HostDirectoryListing {
            ok: true,
            path: Some(path_string(&directory)),
            parent,
            root_selector: false,
            parent_is_root_selector: directory.parent().is_none(),
            entries,
        })
    }

    pub(crate) fn mkdir(&self, parent: &str, name: &str) -> Result<HostDirectoryEntry> {
        let _operation = self
            .filesystem_lock
            .lock()
            .map_err(|_| "Host file operation lock is poisoned")?;
        validate_component(name)?;
        let parent = canonical_directory(parent)?;
        let target = parent.join(name);
        fs::create_dir(&target)?;
        directory_entry(&target)
    }

    pub(crate) fn rename(&self, path: &str, new_name: &str) -> Result<HostDirectoryEntry> {
        let _operation = self
            .filesystem_lock
            .lock()
            .map_err(|_| "Host file operation lock is poisoned")?;
        validate_component(new_name)?;
        let source = existing_path_preserve_leaf(path)?;
        let parent = source.parent().ok_or("Root paths cannot be renamed")?;
        let target = parent.join(new_name);
        if fs::symlink_metadata(&target).is_ok() {
            return Err("A file or directory with that name already exists".into());
        }
        fs::rename(&source, &target)?;
        directory_entry(&target)
    }

    pub(crate) fn prepare_job(
        &self,
        kind: HostFileJobKind,
        sources: Vec<String>,
        destination: Option<String>,
    ) -> Result<HostFileJobSnapshot> {
        if sources.is_empty() {
            return Err("At least one source path is required".into());
        }
        let sources = sources
            .iter()
            .map(|source| existing_path_preserve_leaf(source))
            .collect::<Result<Vec<_>>>()?;
        if sources.iter().any(|source| source.file_name().is_none()) {
            return Err("Filesystem roots cannot be copied, moved, or deleted".into());
        }
        validate_non_overlapping_sources(&sources)?;
        let destination = match kind {
            HostFileJobKind::Copy | HostFileJobKind::Move => Some(canonical_directory(
                destination.ok_or("A destination directory is required")?,
            )?),
            HostFileJobKind::Delete => {
                if destination.is_some() {
                    return Err("Delete jobs do not accept a destination".into());
                }
                None
            }
        };
        let mut source_stats = Vec::with_capacity(sources.len());
        let mut stats = HostFileStats::default();
        for source in &sources {
            let current = collect_stats(source)?;
            add_stats(&mut stats, &current);
            source_stats.push(current);
        }
        let conflicts = if let Some(destination) = destination.as_ref() {
            validate_destination_relationships(&sources, destination)?;
            discover_conflicts(&sources, destination)?
        } else {
            Vec::new()
        };
        let id = self.next_id("file-job");
        let record = Arc::new(Mutex::new(JobRecord {
            id: id.clone(),
            kind,
            state: HostFileJobState::Planning,
            sources,
            source_stats,
            destination,
            stats,
            conflicts,
            processed_items: 0,
            processed_bytes: 0,
            current_path: None,
            succeeded: 0,
            skipped: 0,
            failed: 0,
            results: Vec::new(),
            error: None,
            created_at_ms: now_ms(),
            finished_at_ms: None,
            cancel: Arc::new(AtomicBool::new(false)),
        }));
        {
            let mut locked = record.lock().map_err(|_| "File job lock is poisoned")?;
            locked.state = HostFileJobState::AwaitingConfirmation;
        }
        self.jobs
            .lock()
            .map_err(|_| "File job registry lock is poisoned")?
            .insert(id, Arc::clone(&record));
        job_snapshot(&record)
    }

    pub(crate) fn confirm_job(
        &self,
        operation_id: &str,
        policy: HostFileConflictPolicy,
        replace_directories: bool,
    ) -> Result<HostFileJobSnapshot> {
        let job = self.job(operation_id)?;
        {
            let mut locked = job.lock().map_err(|_| "File job lock is poisoned")?;
            if locked.state != HostFileJobState::AwaitingConfirmation {
                return Err("The file job is not awaiting confirmation".into());
            }
            if policy == HostFileConflictPolicy::Replace
                && locked
                    .conflicts
                    .iter()
                    .any(|conflict| conflict.directory_replacement)
                && !replace_directories
            {
                return Err("Replacing a directory requires explicit confirmation".into());
            }
            locked.state = HostFileJobState::Running;
        }
        let worker_job = Arc::clone(&job);
        let shutdown = Arc::clone(&self.shutdown);
        let filesystem_lock = Arc::clone(&self.filesystem_lock);
        let spawn = thread::Builder::new()
            .name(format!("host-file-job-{operation_id}"))
            .spawn(move || {
                run_job(
                    worker_job,
                    policy,
                    replace_directories,
                    shutdown,
                    filesystem_lock,
                )
            });
        if let Err(error) = spawn {
            if let Ok(mut locked) = job.lock() {
                locked.state = HostFileJobState::Failed;
                locked.error = Some(format!("Unable to start the file job: {error}"));
                locked.finished_at_ms = Some(now_ms());
            }
            return Err(error.into());
        }
        job_snapshot(&job)
    }

    pub(crate) fn job_status(
        &self,
        operation_id: Option<&str>,
    ) -> Result<Vec<HostFileJobSnapshot>> {
        if let Some(operation_id) = operation_id {
            return Ok(vec![job_snapshot(&self.job(operation_id)?)?]);
        }
        let jobs = self
            .jobs
            .lock()
            .map_err(|_| "File job registry lock is poisoned")?;
        let mut snapshots = jobs
            .values()
            .map(job_snapshot)
            .collect::<Result<Vec<_>>>()?;
        snapshots.sort_by_key(|snapshot| snapshot.created_at_ms);
        Ok(snapshots)
    }

    pub(crate) fn cancel_job(&self, operation_id: &str) -> Result<HostFileJobSnapshot> {
        let job = self.job(operation_id)?;
        {
            let mut locked = job.lock().map_err(|_| "File job lock is poisoned")?;
            match locked.state {
                HostFileJobState::Planning
                | HostFileJobState::AwaitingConfirmation
                | HostFileJobState::Running => {
                    locked.cancel.store(true, Ordering::Release);
                    if locked.state != HostFileJobState::Running {
                        locked.state = HostFileJobState::Cancelled;
                        locked.finished_at_ms = Some(now_ms());
                    }
                }
                _ => {}
            }
        }
        job_snapshot(&job)
    }

    pub(crate) fn create_upload(
        &self,
        destination: &str,
        name: &str,
        size_bytes: u64,
        policy: Option<HostFileConflictPolicy>,
    ) -> Result<UploadCreateResult> {
        validate_component(name)?;
        let destination = canonical_directory(destination)?;
        let mut target = destination.join(name);
        let conflict = fs::symlink_metadata(&target)
            .ok()
            .map(|metadata| HostFileConflict {
                source: name.to_owned(),
                target: path_string(&target),
                target_kind: metadata_kind(&metadata).to_owned(),
                directory_replacement: metadata.is_dir(),
            });
        if let Some(conflict) = conflict.clone() {
            let Some(policy) = policy else {
                return Ok(UploadCreateResult {
                    ok: true,
                    requires_confirmation: true,
                    conflict: Some(conflict),
                    upload: None,
                });
            };
            match policy {
                HostFileConflictPolicy::Skip => {
                    return Ok(UploadCreateResult {
                        ok: true,
                        requires_confirmation: false,
                        conflict: Some(conflict),
                        upload: Some(UploadSnapshot {
                            ok: true,
                            upload_id: String::new(),
                            state: "skipped".into(),
                            target_path: path_string(&target),
                            size_bytes,
                            received_bytes: 0,
                            error: None,
                        }),
                    });
                }
                HostFileConflictPolicy::KeepBoth => target = unique_target(&target),
                HostFileConflictPolicy::Replace => {
                    if conflict.directory_replacement {
                        return Err("Uploads cannot replace directories".into());
                    }
                }
            }
        }
        let id = self.next_id("upload");
        let temp_path = unique_temp_path(&destination, &id, "upload");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        let session = UploadSession {
            id: id.clone(),
            temp_path,
            target_path: target.clone(),
            size_bytes,
            received_bytes: 0,
            file,
            policy,
        };
        let snapshot = upload_snapshot(&session, "uploading");
        self.uploads
            .lock()
            .map_err(|_| "Upload registry lock is poisoned")?
            .insert(id, session);
        Ok(UploadCreateResult {
            ok: true,
            requires_confirmation: false,
            conflict,
            upload: Some(snapshot),
        })
    }

    pub(crate) fn upload_chunk(
        &self,
        upload_id: &str,
        offset: u64,
        data: &str,
    ) -> Result<UploadSnapshot> {
        let decoded = BASE64.decode(data)?;
        if decoded.is_empty() || decoded.len() > MAX_UPLOAD_CHUNK_BYTES {
            return Err("Upload chunks must contain at most 512 KiB".into());
        }
        let mut uploads = self
            .uploads
            .lock()
            .map_err(|_| "Upload registry lock is poisoned")?;
        let upload = uploads
            .get_mut(upload_id)
            .ok_or("Upload session not found")?;
        if offset != upload.received_bytes {
            return Err(format!(
                "Unexpected upload offset: expected {}, received {offset}",
                upload.received_bytes
            )
            .into());
        }
        let next = upload
            .received_bytes
            .checked_add(decoded.len() as u64)
            .ok_or("Upload size overflow")?;
        if next > upload.size_bytes {
            return Err("Upload chunk exceeds the declared file size".into());
        }
        upload.file.write_all(&decoded)?;
        upload.received_bytes = next;
        Ok(upload_snapshot(upload, "uploading"))
    }

    pub(crate) fn finish_upload(&self, upload_id: &str) -> Result<UploadSnapshot> {
        let upload = self
            .uploads
            .lock()
            .map_err(|_| "Upload registry lock is poisoned")?
            .remove(upload_id)
            .ok_or("Upload session not found")?;
        let UploadSession {
            id,
            temp_path,
            mut target_path,
            size_bytes,
            received_bytes,
            mut file,
            policy,
        } = upload;
        if received_bytes != size_bytes {
            drop(file);
            let _ = fs::remove_file(&temp_path);
            return Err("Upload is incomplete".into());
        }
        if let Err(error) = file.flush() {
            drop(file);
            let _ = fs::remove_file(&temp_path);
            return Err(error.into());
        }
        if let Err(error) = file.sync_all() {
            drop(file);
            let _ = fs::remove_file(&temp_path);
            return Err(error.into());
        }
        drop(file);
        let _operation = match self.filesystem_lock.lock() {
            Ok(operation) => operation,
            Err(_) => {
                let _ = fs::remove_file(&temp_path);
                return Err("Host file operation lock is poisoned".into());
            }
        };
        let mut replace = false;
        if fs::symlink_metadata(&target_path).is_ok() {
            match policy {
                Some(HostFileConflictPolicy::Replace) => {
                    let metadata = fs::symlink_metadata(&target_path)?;
                    if metadata.is_dir() {
                        let _ = fs::remove_file(&temp_path);
                        return Err("Uploads cannot replace directories".into());
                    }
                    replace = true;
                }
                Some(HostFileConflictPolicy::KeepBoth) => {
                    target_path = unique_target(&target_path);
                }
                _ => {
                    let _ = fs::remove_file(&temp_path);
                    return Err("The upload target changed before completion".into());
                }
            }
        }
        let outcome =
            match commit_temp_path(&temp_path, &target_path, replace, false, &id, "upload") {
                Ok(outcome) => outcome,
                Err(error) => {
                    let _ = fs::remove_file(&temp_path);
                    return Err(error.into());
                }
            };
        let (state, error) = match outcome {
            CommitOutcome::Complete => ("completed", None),
            CommitOutcome::Partial(error) => ("partial", Some(error)),
        };
        Ok(UploadSnapshot {
            ok: true,
            upload_id: id,
            state: state.into(),
            target_path: path_string(&target_path),
            size_bytes,
            received_bytes,
            error,
        })
    }

    pub(crate) fn cancel_upload(&self, upload_id: &str) -> Result<UploadSnapshot> {
        let upload = self
            .uploads
            .lock()
            .map_err(|_| "Upload registry lock is poisoned")?
            .remove(upload_id)
            .ok_or("Upload session not found")?;
        let snapshot = upload_snapshot(&upload, "cancelled");
        drop(upload.file);
        let _ = fs::remove_file(upload.temp_path);
        Ok(snapshot)
    }

    pub(crate) fn create_download(&self, sources: Vec<String>) -> Result<DownloadSnapshot> {
        if sources.is_empty() {
            return Err("At least one download path is required".into());
        }
        let sources = sources
            .iter()
            .map(|source| existing_path_preserve_leaf(source))
            .collect::<Result<Vec<_>>>()?;
        validate_non_overlapping_sources(&sources)?;
        let id = self.next_id("download");
        let created_at_ms = now_ms();
        if sources.len() == 1 {
            let metadata = fs::symlink_metadata(&sources[0])?;
            if metadata.is_file() && !metadata.file_type().is_symlink() {
                let filename = sources[0]
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("download")
                    .to_owned();
                let record = Arc::new(Mutex::new(DownloadRecord {
                    id: id.clone(),
                    state: "ready".into(),
                    filename,
                    path: Some(sources[0].clone()),
                    owned_temp: false,
                    size_bytes: Some(metadata.len()),
                    error: None,
                    created_at_ms,
                    cancel: Arc::new(AtomicBool::new(false)),
                }));
                let snapshot = download_snapshot(&record)?;
                self.downloads
                    .lock()
                    .map_err(|_| "Download registry lock is poisoned")?
                    .insert(id, record);
                return Ok(snapshot);
            }
        }
        for source in &sources {
            let metadata = fs::symlink_metadata(source)?;
            if !metadata.is_file() && !metadata.is_dir() && !metadata.file_type().is_symlink() {
                return Err("Special files cannot be downloaded".into());
            }
        }
        let filename = if sources.len() == 1 {
            format!(
                "{}.tar.gz",
                sources[0]
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("download")
            )
        } else {
            format!("me-files-{created_at_ms}.tar.gz")
        };
        let temp_path = std::env::temp_dir().join(format!("me-{id}.tar.gz"));
        let record = Arc::new(Mutex::new(DownloadRecord {
            id: id.clone(),
            state: "preparing".into(),
            filename,
            path: Some(temp_path.clone()),
            owned_temp: true,
            size_bytes: None,
            error: None,
            created_at_ms,
            cancel: Arc::new(AtomicBool::new(false)),
        }));
        self.downloads
            .lock()
            .map_err(|_| "Download registry lock is poisoned")?
            .insert(id.clone(), Arc::clone(&record));
        let shutdown = Arc::clone(&self.shutdown);
        let worker_record = Arc::clone(&record);
        let spawn = thread::Builder::new()
            .name(format!("host-file-download-{id}"))
            .spawn(move || prepare_archive(worker_record, sources, temp_path, shutdown));
        if let Err(error) = spawn
            && let Ok(mut locked) = record.lock()
        {
            locked.state = "failed".into();
            locked.error = Some(format!("Unable to prepare the download: {error}"));
        }
        download_snapshot(&record)
    }

    pub(crate) fn download_status(&self, download_id: &str) -> Result<DownloadSnapshot> {
        download_snapshot(&self.download(download_id)?)
    }

    pub(crate) fn cancel_download(&self, download_id: &str) -> Result<DownloadSnapshot> {
        let download = self.download(download_id)?;
        let (owned_temp, path, snapshot) = {
            let mut locked = download
                .lock()
                .map_err(|_| "Download session lock is poisoned")?;
            locked.cancel.store(true, Ordering::Release);
            locked.state = "cancelled".into();
            (
                locked.owned_temp,
                locked.path.clone(),
                download_snapshot_locked(&locked),
            )
        };
        if owned_temp && let Some(path) = path {
            let _ = fs::remove_file(path);
        }
        Ok(snapshot)
    }

    pub(crate) fn open_download(
        &self,
        download_id: &str,
        range: Option<&str>,
    ) -> Result<DownloadStream> {
        let download = self.download(download_id)?;
        let (path, filename, expected_size) = {
            let locked = download
                .lock()
                .map_err(|_| "Download session lock is poisoned")?;
            if locked.state != "ready" {
                return Err("Download is not ready".into());
            }
            (
                locked.path.clone().ok_or("Download file is unavailable")?,
                locked.filename.clone(),
                locked.size_bytes,
            )
        };
        let mut file = File::open(&path)?;
        let total_length = file.metadata()?.len();
        if expected_size.is_some_and(|size| size != total_length) {
            return Err("Download file changed before transfer".into());
        }
        let (start, end, partial) = parse_range(range, total_length)?;
        file.seek(SeekFrom::Start(start))?;
        let length = if total_length == 0 {
            0
        } else {
            end.saturating_sub(start).saturating_add(1)
        };
        let content_length = usize::try_from(length).map_err(|_| "Download range is too large")?;
        Ok(DownloadStream {
            reader: Box::new(file.take(length)),
            status: if partial { 206 } else { 200 },
            content_length,
            content_range: partial.then(|| format!("bytes {start}-{end}/{total_length}")),
            filename,
            content_type: if path.extension().and_then(|extension| extension.to_str()) == Some("gz")
            {
                "application/gzip"
            } else {
                "application/octet-stream"
            },
        })
    }

    pub(crate) fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        if let Ok(jobs) = self.jobs.lock() {
            for job in jobs.values() {
                if let Ok(job) = job.lock() {
                    job.cancel.store(true, Ordering::Release);
                }
            }
        }
        if let Ok(mut uploads) = self.uploads.lock() {
            for (_, upload) in uploads.drain() {
                drop(upload.file);
                let _ = fs::remove_file(upload.temp_path);
            }
        }
        if let Ok(downloads) = self.downloads.lock() {
            for download in downloads.values() {
                if let Ok(download) = download.lock() {
                    download.cancel.store(true, Ordering::Release);
                    if download.owned_temp
                        && let Some(path) = download.path.as_ref()
                    {
                        let _ = fs::remove_file(path);
                    }
                }
            }
        }
    }

    fn job(&self, operation_id: &str) -> Result<Arc<Mutex<JobRecord>>> {
        self.jobs
            .lock()
            .map_err(|_| "File job registry lock is poisoned")?
            .get(operation_id)
            .cloned()
            .ok_or_else(|| "File job not found".into())
    }

    fn download(&self, download_id: &str) -> Result<Arc<Mutex<DownloadRecord>>> {
        self.downloads
            .lock()
            .map_err(|_| "Download registry lock is poisoned")?
            .get(download_id)
            .cloned()
            .ok_or_else(|| "Download session not found".into())
    }

    fn next_id(&self, prefix: &str) -> String {
        let sequence = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{:x}-{sequence:x}", now_ms())
    }
}

impl Drop for HostFileManager {
    fn drop(&mut self) {
        if Arc::strong_count(&self.shutdown) == 1 {
            self.shutdown();
        }
    }
}

fn run_job(
    job: Arc<Mutex<JobRecord>>,
    policy: HostFileConflictPolicy,
    replace_directories: bool,
    shutdown: Arc<AtomicBool>,
    filesystem_lock: Arc<Mutex<()>>,
) {
    let _operation = match filesystem_lock.lock() {
        Ok(operation) => operation,
        Err(_) => {
            if let Ok(mut locked) = job.lock() {
                locked.state = HostFileJobState::Failed;
                locked.error = Some("Host file operation lock is poisoned".into());
                locked.finished_at_ms = Some(now_ms());
            }
            return;
        }
    };
    let (kind, sources, source_stats, destination, cancel, id) = match job.lock() {
        Ok(locked) => (
            locked.kind,
            locked.sources.clone(),
            locked.source_stats.clone(),
            locked.destination.clone(),
            Arc::clone(&locked.cancel),
            locked.id.clone(),
        ),
        Err(_) => return,
    };
    for (index, source) in sources.iter().enumerate() {
        if cancelled(&cancel, &shutdown) {
            finish_cancelled(&job);
            return;
        }
        let processed_before = job.lock().map(|locked| locked.processed_items).unwrap_or(0);
        set_current_path(&job, source);
        let result = match kind {
            HostFileJobKind::Copy => execute_copy(
                source,
                destination.as_ref().expect("copy destination"),
                &id,
                policy,
                replace_directories,
                &job,
                &cancel,
                &shutdown,
                false,
                &source_stats[index],
            ),
            HostFileJobKind::Move => execute_copy(
                source,
                destination.as_ref().expect("move destination"),
                &id,
                policy,
                replace_directories,
                &job,
                &cancel,
                &shutdown,
                true,
                &source_stats[index],
            ),
            HostFileJobKind::Delete => {
                execute_delete(source, &job, &cancel, &shutdown).map(|_| HostFileItemResult {
                    source: path_string(source),
                    target: None,
                    status: "succeeded".into(),
                    error: None,
                })
            }
        };
        let delete_changed = kind == HostFileJobKind::Delete
            && job
                .lock()
                .is_ok_and(|locked| locked.processed_items > processed_before);
        match result {
            Ok(item) => push_result(&job, item),
            Err(error) if error == "cancelled" => {
                if delete_changed {
                    push_result(
                        &job,
                        HostFileItemResult {
                            source: path_string(source),
                            target: None,
                            status: "partial".into(),
                            error: Some(
                                "Deletion was cancelled after some contents were permanently removed"
                                    .into(),
                            ),
                        },
                    );
                }
                finish_cancelled(&job);
                return;
            }
            Err(error) => push_result(
                &job,
                HostFileItemResult {
                    source: path_string(source),
                    target: None,
                    status: if delete_changed {
                        "partial".into()
                    } else {
                        "failed".into()
                    },
                    error: Some(error),
                },
            ),
        }
    }
    if let Ok(mut locked) = job.lock() {
        locked.current_path = None;
        locked.finished_at_ms = Some(now_ms());
        if locked.failed > 0 {
            locked.state = HostFileJobState::Failed;
            locked.error = Some(if locked.succeeded > 0 || locked.skipped > 0 {
                "The file job completed with partial failures".into()
            } else {
                "The file job failed".into()
            });
        } else {
            locked.state = HostFileJobState::Completed;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_copy(
    source: &Path,
    destination: &Path,
    id: &str,
    policy: HostFileConflictPolicy,
    replace_directories: bool,
    job: &Arc<Mutex<JobRecord>>,
    cancel: &AtomicBool,
    shutdown: &AtomicBool,
    moving: bool,
    source_stats: &HostFileStats,
) -> std::result::Result<HostFileItemResult, String> {
    let name = source
        .file_name()
        .ok_or_else(|| "Root paths cannot be copied or moved".to_owned())?;
    let requested_target = destination.join(name);
    let (target, replace) = match resolve_target(&requested_target, policy, replace_directories)? {
        Some(value) => value,
        None => {
            advance_progress(job, source_stats.items, source_stats.bytes);
            return Ok(HostFileItemResult {
                source: path_string(source),
                target: Some(path_string(&requested_target)),
                status: "skipped".into(),
                error: None,
            });
        }
    };
    if moving && !replace && fs::rename(source, &target).is_ok() {
        advance_progress(job, source_stats.items, source_stats.bytes);
        return Ok(HostFileItemResult {
            source: path_string(source),
            target: Some(path_string(&target)),
            status: "succeeded".into(),
            error: None,
        });
    }
    let parent = target
        .parent()
        .ok_or_else(|| "Destination has no parent directory".to_owned())?;
    let temp = unique_temp_path(parent, id, "copy");
    if let Err(error) = copy_tree(source, &temp, job, cancel, shutdown) {
        let _ = remove_path(&temp);
        return Err(error);
    }
    if cancelled(cancel, shutdown) {
        let _ = remove_path(&temp);
        return Err("cancelled".into());
    }
    let commit = match commit_temp_path(&temp, &target, replace, replace_directories, id, "copy") {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = remove_path(&temp);
            return Err(error);
        }
    };
    let mut errors = Vec::new();
    if let CommitOutcome::Partial(error) = commit {
        errors.push(error);
    }
    if moving && let Err(remove_error) = remove_tree_untracked(source, cancel, shutdown) {
        errors.push(format!(
            "The destination was created, but the source could not be removed: {remove_error}"
        ));
    }
    Ok(HostFileItemResult {
        source: path_string(source),
        target: Some(path_string(&target)),
        status: if errors.is_empty() {
            "succeeded".into()
        } else {
            "partial".into()
        },
        error: (!errors.is_empty()).then(|| errors.join("; ")),
    })
}

fn execute_delete(
    source: &Path,
    job: &Arc<Mutex<JobRecord>>,
    cancel: &AtomicBool,
    shutdown: &AtomicBool,
) -> std::result::Result<(), String> {
    remove_tree(source, job, cancel, shutdown)
}

fn copy_tree(
    source: &Path,
    target: &Path,
    job: &Arc<Mutex<JobRecord>>,
    cancel: &AtomicBool,
    shutdown: &AtomicBool,
) -> std::result::Result<(), String> {
    if cancelled(cancel, shutdown) {
        return Err("cancelled".into());
    }
    set_current_path(job, source);
    let metadata = fs::symlink_metadata(source).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() {
        copy_symlink(source, target).map_err(|error| error.to_string())?;
        advance_progress(job, 1, 0);
    } else if metadata.is_file() {
        let mut input = File::open(source).map_err(|error| error.to_string())?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)
            .map_err(|error| error.to_string())?;
        let mut buffer = vec![0; COPY_BUFFER_BYTES];
        loop {
            if cancelled(cancel, shutdown) {
                return Err("cancelled".into());
            }
            let read = input.read(&mut buffer).map_err(|error| error.to_string())?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .map_err(|error| error.to_string())?;
            advance_progress(job, 0, read as u64);
        }
        output.flush().map_err(|error| error.to_string())?;
        let _ = fs::set_permissions(target, metadata.permissions());
        advance_progress(job, 1, 0);
    } else if metadata.is_dir() {
        fs::create_dir(target).map_err(|error| error.to_string())?;
        for entry in fs::read_dir(source).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            copy_tree(
                &entry.path(),
                &target.join(entry.file_name()),
                job,
                cancel,
                shutdown,
            )?;
        }
        let _ = fs::set_permissions(target, metadata.permissions());
        advance_progress(job, 1, 0);
    } else {
        return Err(format!("Unsupported special file: {}", source.display()));
    }
    Ok(())
}

fn remove_tree(
    path: &Path,
    job: &Arc<Mutex<JobRecord>>,
    cancel: &AtomicBool,
    shutdown: &AtomicBool,
) -> std::result::Result<(), String> {
    if cancelled(cancel, shutdown) {
        return Err("cancelled".into());
    }
    set_current_path(job, path);
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        fs::remove_file(path).map_err(|error| error.to_string())?;
        advance_progress(
            job,
            1,
            if metadata.is_file() {
                metadata.len()
            } else {
                0
            },
        );
    } else if metadata.is_dir() {
        for entry in fs::read_dir(path).map_err(|error| error.to_string())? {
            remove_tree(
                &entry.map_err(|error| error.to_string())?.path(),
                job,
                cancel,
                shutdown,
            )?;
        }
        fs::remove_dir(path).map_err(|error| error.to_string())?;
        advance_progress(job, 1, 0);
    } else {
        fs::remove_file(path).map_err(|error| error.to_string())?;
        advance_progress(job, 1, 0);
    }
    Ok(())
}

fn remove_tree_untracked(
    path: &Path,
    cancel: &AtomicBool,
    shutdown: &AtomicBool,
) -> std::result::Result<(), String> {
    if cancelled(cancel, shutdown) {
        return Err("cancelled".into());
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || metadata.is_file() || !metadata.is_dir() {
        fs::remove_file(path).map_err(|error| error.to_string())?;
    } else {
        for entry in fs::read_dir(path).map_err(|error| error.to_string())? {
            remove_tree_untracked(
                &entry.map_err(|error| error.to_string())?.path(),
                cancel,
                shutdown,
            )?;
        }
        fs::remove_dir(path).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn resolve_target(
    requested: &Path,
    policy: HostFileConflictPolicy,
    replace_directories: bool,
) -> std::result::Result<Option<(PathBuf, bool)>, String> {
    let Ok(metadata) = fs::symlink_metadata(requested) else {
        return Ok(Some((requested.to_owned(), false)));
    };
    match policy {
        HostFileConflictPolicy::Skip => Ok(None),
        HostFileConflictPolicy::KeepBoth => Ok(Some((unique_target(requested), false))),
        HostFileConflictPolicy::Replace => {
            if metadata.is_dir() && !replace_directories {
                Err("Replacing a directory requires explicit confirmation".into())
            } else {
                Ok(Some((requested.to_owned(), true)))
            }
        }
    }
}

fn prepare_archive(
    record: Arc<Mutex<DownloadRecord>>,
    sources: Vec<PathBuf>,
    temp_path: PathBuf,
    shutdown: Arc<AtomicBool>,
) {
    let cancel = match record.lock() {
        Ok(locked) => Arc::clone(&locked.cancel),
        Err(_) => return,
    };
    let result = (|| -> Result<u64> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        let encoder = GzEncoder::new(file, Compression::best());
        let mut archive = Builder::new(encoder);
        archive.follow_symlinks(false);
        let mut used_names = HashSet::new();
        for source in sources {
            if cancelled(&cancel, &shutdown) {
                return Err("cancelled".into());
            }
            let base = source
                .file_name()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("root"));
            let archive_name = unique_archive_name(base, &mut used_names);
            append_archive_entry(&mut archive, &source, &archive_name, &cancel, &shutdown)?;
        }
        let encoder = archive.into_inner()?;
        let mut file = encoder.finish()?;
        file.flush()?;
        file.sync_all()?;
        Ok(file.metadata()?.len())
    })();
    match record.lock() {
        Ok(mut locked) => match result {
            Ok(size) if !cancelled(&cancel, &shutdown) => {
                locked.state = "ready".into();
                locked.size_bytes = Some(size);
            }
            Ok(_) => {
                locked.state = "cancelled".into();
                let _ = fs::remove_file(&temp_path);
            }
            Err(error) => {
                let message = error.to_string();
                locked.state = if message == "cancelled" {
                    "cancelled".into()
                } else {
                    "failed".into()
                };
                locked.error = (message != "cancelled").then_some(message);
                let _ = fs::remove_file(&temp_path);
            }
        },
        Err(_) => {
            let _ = fs::remove_file(&temp_path);
        }
    }
}

fn append_archive_entry<W: Write>(
    archive: &mut Builder<W>,
    source: &Path,
    archive_path: &Path,
    cancel: &AtomicBool,
    shutdown: &AtomicBool,
) -> Result<()> {
    if cancelled(cancel, shutdown) {
        return Err("cancelled".into());
    }
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        archive.append_dir(archive_path, source)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            append_archive_entry(
                archive,
                &entry.path(),
                &archive_path.join(entry.file_name()),
                cancel,
                shutdown,
            )?;
        }
    } else if metadata.is_file() || metadata.file_type().is_symlink() {
        archive.append_path_with_name(source, archive_path)?;
    } else {
        return Err(format!("Special files cannot be archived: {}", source.display()).into());
    }
    Ok(())
}

fn job_snapshot(job: &Arc<Mutex<JobRecord>>) -> Result<HostFileJobSnapshot> {
    let locked = job.lock().map_err(|_| "File job lock is poisoned")?;
    Ok(HostFileJobSnapshot {
        ok: true,
        operation_id: locked.id.clone(),
        kind: locked.kind,
        state: locked.state,
        sources: locked
            .sources
            .iter()
            .map(|path| path_string(path))
            .collect(),
        destination: locked.destination.as_ref().map(|path| path_string(path)),
        stats: locked.stats.clone(),
        conflicts: locked.conflicts.clone(),
        processed_items: locked.processed_items,
        processed_bytes: locked.processed_bytes,
        current_path: locked.current_path.as_ref().map(|path| path_string(path)),
        succeeded: locked.succeeded,
        skipped: locked.skipped,
        failed: locked.failed,
        results: locked.results.clone(),
        error: locked.error.clone(),
        created_at_ms: locked.created_at_ms,
        finished_at_ms: locked.finished_at_ms,
        cancellable: matches!(
            locked.state,
            HostFileJobState::Planning
                | HostFileJobState::AwaitingConfirmation
                | HostFileJobState::Running
        ),
    })
}

fn push_result(job: &Arc<Mutex<JobRecord>>, item: HostFileItemResult) {
    if let Ok(mut locked) = job.lock() {
        match item.status.as_str() {
            "succeeded" => locked.succeeded += 1,
            "skipped" => locked.skipped += 1,
            _ => locked.failed += 1,
        }
        locked.results.push(item);
    }
}

fn set_current_path(job: &Arc<Mutex<JobRecord>>, path: &Path) {
    if let Ok(mut locked) = job.lock() {
        locked.current_path = Some(path.to_owned());
    }
}

fn advance_progress(job: &Arc<Mutex<JobRecord>>, items: u64, bytes: u64) {
    if let Ok(mut locked) = job.lock() {
        locked.processed_items = locked.processed_items.saturating_add(items);
        locked.processed_bytes = locked.processed_bytes.saturating_add(bytes);
    }
}

fn finish_cancelled(job: &Arc<Mutex<JobRecord>>) {
    if let Ok(mut locked) = job.lock() {
        locked.state = HostFileJobState::Cancelled;
        locked.current_path = None;
        locked.finished_at_ms = Some(now_ms());
    }
}

fn cancelled(cancel: &AtomicBool, shutdown: &AtomicBool) -> bool {
    cancel.load(Ordering::Acquire) || shutdown.load(Ordering::Acquire)
}

fn collect_stats(path: &Path) -> Result<HostFileStats> {
    let metadata = fs::symlink_metadata(path)?;
    let mut stats = HostFileStats {
        items: 1,
        ..HostFileStats::default()
    };
    if metadata.file_type().is_symlink() {
        stats.symlinks = 1;
    } else if metadata.is_file() {
        stats.files = 1;
        stats.bytes = metadata.len();
    } else if metadata.is_dir() {
        stats.directories = 1;
        for entry in fs::read_dir(path)? {
            add_stats(&mut stats, &collect_stats(&entry?.path())?);
        }
    } else {
        stats.files = 1;
    }
    Ok(stats)
}

fn add_stats(total: &mut HostFileStats, value: &HostFileStats) {
    total.items = total.items.saturating_add(value.items);
    total.files = total.files.saturating_add(value.files);
    total.directories = total.directories.saturating_add(value.directories);
    total.symlinks = total.symlinks.saturating_add(value.symlinks);
    total.bytes = total.bytes.saturating_add(value.bytes);
}

fn discover_conflicts(sources: &[PathBuf], destination: &Path) -> Result<Vec<HostFileConflict>> {
    let mut conflicts = Vec::new();
    let mut targets = HashSet::new();
    for source in sources {
        let name = source
            .file_name()
            .ok_or("Root paths cannot be copied or moved")?;
        let target = destination.join(name);
        if !targets.insert(target.clone()) {
            conflicts.push(HostFileConflict {
                source: path_string(source),
                target: path_string(&target),
                target_kind: "batch".into(),
                directory_replacement: false,
            });
        } else if let Ok(metadata) = fs::symlink_metadata(&target) {
            conflicts.push(HostFileConflict {
                source: path_string(source),
                target: path_string(&target),
                target_kind: metadata_kind(&metadata).to_owned(),
                directory_replacement: metadata.is_dir(),
            });
        }
    }
    Ok(conflicts)
}

fn validate_destination_relationships(sources: &[PathBuf], destination: &Path) -> Result<()> {
    for source in sources {
        let metadata = fs::symlink_metadata(source)?;
        let name = source
            .file_name()
            .ok_or("Root paths cannot be copied or moved")?;
        let target = destination.join(name);
        if target == *source {
            return Err("A source and destination resolve to the same path".into());
        }
        if metadata.is_dir() && (destination.starts_with(source) || target.starts_with(source)) {
            return Err(
                "A directory cannot be copied or moved into itself or its descendants".into(),
            );
        }
    }
    Ok(())
}

fn validate_non_overlapping_sources(sources: &[PathBuf]) -> Result<()> {
    for (index, left) in sources.iter().enumerate() {
        for right in sources.iter().skip(index + 1) {
            if left == right || left.starts_with(right) || right.starts_with(left) {
                return Err("Source paths must not duplicate or contain one another".into());
            }
        }
    }
    Ok(())
}

fn canonical_directory(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = fs::canonicalize(path.as_ref())?;
    if !path.is_dir() {
        return Err("Path is not a directory".into());
    }
    Ok(path)
}

fn existing_path_preserve_leaf(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_internal_temp_name)
    {
        return Err("Path is reserved for an active file transfer".into());
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        let parent = path.parent().ok_or("Path has no parent")?;
        let parent = fs::canonicalize(parent)?;
        let name = path.file_name().ok_or("Path has no file name")?;
        Ok(parent.join(name))
    } else {
        Ok(fs::canonicalize(path)?)
    }
}

fn validate_component(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('\0')
        || is_internal_temp_name(name)
    {
        return Err("Name must be a single valid path component".into());
    }
    let path = Path::new(name);
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err("Name must be a single valid path component".into());
    }
    #[cfg(windows)]
    if name.contains('/') || name.contains('\\') || name.contains(':') {
        return Err("Name must be a single valid path component".into());
    }
    Ok(())
}
fn is_internal_temp_name(name: &str) -> bool {
    (name.starts_with(".me-upload-") || name.starts_with(".me-copy-")) && name.ends_with(".tmp")
}

fn directory_entry(path: &Path) -> Result<HostDirectoryEntry> {
    let metadata = fs::symlink_metadata(path)?;
    let symlink = metadata.file_type().is_symlink();
    let navigable = if symlink {
        fs::metadata(path).is_ok_and(|target| target.is_dir())
    } else {
        metadata.is_dir()
    };
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    Ok(HostDirectoryEntry {
        hidden: metadata_hidden(&name, &metadata),
        name,
        path: path_string(path),
        kind: metadata_kind(&metadata).to_owned(),
        size_bytes: metadata.is_file().then_some(metadata.len()),
        modified_at_ms: metadata.modified().ok().map(system_time_ms),
        readonly: metadata.permissions().readonly(),
        symlink,
        navigable,
    })
}

fn metadata_hidden(name: &str, metadata: &fs::Metadata) -> bool {
    let dot_hidden = name.starts_with('.');
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        dot_hidden || metadata.file_attributes() & 0x2 != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        dot_hidden
    }
}

fn metadata_kind(metadata: &fs::Metadata) -> &'static str {
    if metadata.file_type().is_symlink() {
        "symlink"
    } else if metadata.is_dir() {
        "directory"
    } else if metadata.is_file() {
        "file"
    } else {
        "special"
    }
}

#[cfg(unix)]
fn host_roots() -> Result<Vec<HostDirectoryEntry>> {
    let metadata = fs::metadata("/")?;
    Ok(vec![HostDirectoryEntry {
        name: "/".into(),
        path: "/".into(),
        kind: "root".into(),
        size_bytes: None,
        modified_at_ms: metadata.modified().ok().map(system_time_ms),
        hidden: false,
        readonly: metadata.permissions().readonly(),
        symlink: false,
        navigable: true,
    }])
}

#[cfg(windows)]
fn host_roots() -> Result<Vec<HostDirectoryEntry>> {
    let mut roots = Vec::new();
    for letter in b'A'..=b'Z' {
        let path = format!("{}:\\", letter as char);
        if let Ok(metadata) = fs::metadata(&path) {
            roots.push(HostDirectoryEntry {
                name: format!("{}:", letter as char),
                path,
                kind: "drive".into(),
                size_bytes: None,
                modified_at_ms: metadata.modified().ok().map(system_time_ms),
                hidden: false,
                readonly: metadata.permissions().readonly(),
                symlink: false,
                navigable: true,
            });
        }
    }
    Ok(roots)
}

#[cfg(unix)]
fn copy_symlink(source: &Path, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(fs::read_link(source)?, target)
}

#[cfg(windows)]
fn copy_symlink(source: &Path, target: &Path) -> std::io::Result<()> {
    let value = fs::read_link(source)?;
    if fs::metadata(source).is_ok_and(|metadata| metadata.is_dir()) {
        std::os::windows::fs::symlink_dir(value, target)
    } else {
        std::os::windows::fs::symlink_file(value, target)
    }
}

fn remove_path(path: &Path) -> Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() || metadata.is_file() || !metadata.is_dir() {
        fs::remove_file(path)?;
    } else {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

fn commit_temp_path(
    temp: &Path,
    target: &Path,
    replace: bool,
    replace_directories: bool,
    id: &str,
    kind: &str,
) -> std::result::Result<CommitOutcome, String> {
    if !replace {
        if fs::symlink_metadata(target).is_ok() {
            return Err("The destination changed while the job was running".into());
        }
        fs::rename(temp, target).map_err(|error| error.to_string())?;
        return Ok(CommitOutcome::Complete);
    }
    let metadata = match fs::symlink_metadata(target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::rename(temp, target).map_err(|error| error.to_string())?;
            return Ok(CommitOutcome::Complete);
        }
        Err(error) => return Err(error.to_string()),
    };
    if metadata.is_dir() && !replace_directories {
        return Err("Replacing a directory requires explicit confirmation".into());
    }
    let parent = target
        .parent()
        .ok_or_else(|| "Destination has no parent directory".to_owned())?;
    let backup = unique_temp_path(parent, id, kind);
    fs::rename(target, &backup).map_err(|error| error.to_string())?;
    if let Err(commit_error) = fs::rename(temp, target) {
        return match fs::rename(&backup, target) {
            Ok(()) => Err(format!(
                "Unable to commit the replacement; the previous target was restored: {commit_error}"
            )),
            Err(restore_error) => Err(format!(
                "Unable to commit the replacement or restore the previous target; the previous target remains at {}: {commit_error}; {restore_error}",
                backup.display()
            )),
        };
    }
    match remove_path(&backup) {
        Ok(()) => Ok(CommitOutcome::Complete),
        Err(error) => Ok(CommitOutcome::Partial(format!(
            "The destination was committed, but the previous target backup could not be fully removed: {error}"
        ))),
    }
}

fn unique_target(requested: &Path) -> PathBuf {
    let parent = requested.parent().unwrap_or_else(|| Path::new("."));
    let stem = requested
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("item");
    let extension = requested
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{value}"))
        .unwrap_or_default();
    for number in 1..u32::MAX {
        let suffix = if number == 1 {
            " copy".to_owned()
        } else {
            format!(" copy {number}")
        };
        let candidate = parent.join(format!("{stem}{suffix}{extension}"));
        if fs::symlink_metadata(&candidate).is_err() {
            return candidate;
        }
    }
    parent.join(format!("{stem} copy-{}{}", now_ms(), extension))
}

fn unique_temp_path(parent: &Path, id: &str, kind: &str) -> PathBuf {
    for sequence in 0..u32::MAX {
        let candidate = parent.join(format!(".me-{kind}-{id}-{sequence}.tmp"));
        if fs::symlink_metadata(&candidate).is_err() {
            return candidate;
        }
    }
    parent.join(format!(".me-{kind}-{id}-{}.tmp", now_ms()))
}

fn unique_archive_name(mut name: PathBuf, used: &mut HashSet<PathBuf>) -> PathBuf {
    if used.insert(name.clone()) {
        return name;
    }
    let original = name.clone();
    for index in 2..u32::MAX {
        name = PathBuf::from(format!("{}-{index}", original.display()));
        if used.insert(name.clone()) {
            return name;
        }
    }
    PathBuf::from(format!("{}-{}", original.display(), now_ms()))
}

fn parse_range(range: Option<&str>, total: u64) -> Result<(u64, u64, bool)> {
    if total == 0 {
        if range.is_some() {
            return Err("Range is not satisfiable".into());
        }
        return Ok((0, 0, false));
    }
    let Some(range) = range else {
        return Ok((0, total - 1, false));
    };
    let value = range
        .strip_prefix("bytes=")
        .ok_or("Only byte ranges are supported")?;
    if value.contains(',') {
        return Err("Multiple ranges are not supported".into());
    }
    let (start, end) = value.split_once('-').ok_or("Invalid byte range")?;
    let (start, end) = if start.is_empty() {
        let suffix = end.parse::<u64>()?;
        if suffix == 0 {
            return Err("Range is not satisfiable".into());
        }
        (total.saturating_sub(suffix), total - 1)
    } else {
        let start = start.parse::<u64>()?;
        let end = if end.is_empty() {
            total - 1
        } else {
            end.parse::<u64>()?.min(total - 1)
        };
        (start, end)
    };
    if start >= total || end < start {
        return Err("Range is not satisfiable".into());
    }
    Ok((start, end, true))
}

fn upload_snapshot(upload: &UploadSession, state: &str) -> UploadSnapshot {
    UploadSnapshot {
        ok: true,
        upload_id: upload.id.clone(),
        state: state.into(),
        target_path: path_string(&upload.target_path),
        size_bytes: upload.size_bytes,
        received_bytes: upload.received_bytes,
        error: None,
    }
}

fn download_snapshot(record: &Arc<Mutex<DownloadRecord>>) -> Result<DownloadSnapshot> {
    let locked = record
        .lock()
        .map_err(|_| "Download session lock is poisoned")?;
    Ok(download_snapshot_locked(&locked))
}

fn download_snapshot_locked(record: &DownloadRecord) -> DownloadSnapshot {
    DownloadSnapshot {
        ok: true,
        download_id: record.id.clone(),
        state: record.state.clone(),
        filename: record.filename.clone(),
        size_bytes: record.size_bytes,
        error: record.error.clone(),
        created_at_ms: record.created_at_ms,
    }
}

fn path_string(path: impl AsRef<Path>) -> String {
    crate::host_path::public_host_path(path)
}

fn now_ms() -> u64 {
    system_time_ms(SystemTime::now())
}

fn system_time_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Fixture {
        path: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "me-host-files-{name}-{}-{}",
                std::process::id(),
                now_ms()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn wait_for_job(manager: &HostFileManager, id: &str) -> HostFileJobSnapshot {
        for _ in 0..200 {
            let snapshot = manager.job_status(Some(id)).unwrap().remove(0);
            if matches!(
                snapshot.state,
                HostFileJobState::Completed
                    | HostFileJobState::Failed
                    | HostFileJobState::Cancelled
            ) {
                return snapshot;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("file job did not finish");
    }

    #[test]
    fn directory_operations_and_jobs_are_server_owned() {
        let fixture = Fixture::new("jobs");
        let manager = HostFileManager::new(&fixture.path).unwrap();
        let source = fixture.path.join("source");
        let target = fixture.path.join("target");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(source.join("nested/file.txt"), b"hello").unwrap();

        let listing = manager.list(None, false).unwrap();
        assert_eq!(
            listing.path.as_deref(),
            Some(path_string(fs::canonicalize(&fixture.path).unwrap()).as_str())
        );
        assert!(listing.entries.iter().any(|entry| entry.name == "source"));

        let created = manager
            .mkdir(&path_string(&fixture.path), "created")
            .unwrap();
        let renamed = manager.rename(&created.path, "renamed").unwrap();
        assert!(Path::new(&renamed.path).is_dir());

        let prepared = manager
            .prepare_job(
                HostFileJobKind::Copy,
                vec![path_string(&source)],
                Some(path_string(&target)),
            )
            .unwrap();
        assert_eq!(prepared.state, HostFileJobState::AwaitingConfirmation);
        manager
            .confirm_job(&prepared.operation_id, HostFileConflictPolicy::Skip, false)
            .unwrap();
        let completed = wait_for_job(&manager, &prepared.operation_id);
        assert_eq!(completed.state, HostFileJobState::Completed);
        assert_eq!(
            fs::read(target.join("source/nested/file.txt")).unwrap(),
            b"hello"
        );
    }

    #[test]
    fn rejects_copying_a_directory_into_its_descendant() {
        let fixture = Fixture::new("descendant");
        let manager = HostFileManager::new(&fixture.path).unwrap();
        let source = fixture.path.join("source");
        let descendant = source.join("descendant");
        fs::create_dir_all(&descendant).unwrap();
        let error = manager
            .prepare_job(
                HostFileJobKind::Copy,
                vec![path_string(&source)],
                Some(path_string(&descendant)),
            )
            .unwrap_err();
        assert!(error.to_string().contains("descendants"));
    }

    #[test]
    fn upload_chunks_and_range_download_preserve_bytes() {
        let fixture = Fixture::new("transfer");
        let manager = HostFileManager::new(&fixture.path).unwrap();
        let created = manager
            .create_upload(&path_string(&fixture.path), "payload.bin", 6, None)
            .unwrap();
        let upload = created.upload.unwrap();
        manager
            .upload_chunk(&upload.upload_id, 0, &BASE64.encode(b"abcdef"))
            .unwrap();
        manager.finish_upload(&upload.upload_id).unwrap();

        let download = manager
            .create_download(vec![path_string(fixture.path.join("payload.bin"))])
            .unwrap();
        let mut stream = manager
            .open_download(&download.download_id, Some("bytes=1-3"))
            .unwrap();
        let mut body = Vec::new();
        stream.reader.read_to_end(&mut body).unwrap();
        assert_eq!(body, b"bcd");
        assert_eq!(stream.status, 206);
        assert_eq!(stream.content_range.as_deref(), Some("bytes 1-3/6"));
    }

    #[cfg(unix)]
    #[test]
    fn recursive_delete_does_not_follow_symlinks() {
        let fixture = Fixture::new("symlink");
        let manager = HostFileManager::new(&fixture.path).unwrap();
        let outside = fixture.path.join("outside");
        let selected = fixture.path.join("selected");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&selected).unwrap();
        fs::write(outside.join("keep.txt"), b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, selected.join("link")).unwrap();
        let prepared = manager
            .prepare_job(HostFileJobKind::Delete, vec![path_string(&selected)], None)
            .unwrap();
        manager
            .confirm_job(&prepared.operation_id, HostFileConflictPolicy::Skip, false)
            .unwrap();
        let completed = wait_for_job(&manager, &prepared.operation_id);
        assert_eq!(completed.state, HostFileJobState::Completed);
        assert_eq!(fs::read(outside.join("keep.txt")).unwrap(), b"keep");
    }

    #[test]
    fn conflict_policies_and_directory_replacement_are_explicit() {
        let fixture = Fixture::new("conflicts");
        let manager = HostFileManager::new(&fixture.path).unwrap();
        let source_root = fixture.path.join("source");
        let target_root = fixture.path.join("target");
        fs::create_dir_all(source_root.join("folder")).unwrap();
        fs::create_dir_all(target_root.join("folder")).unwrap();
        fs::write(source_root.join("folder/new.txt"), b"new").unwrap();
        fs::write(target_root.join("folder/old.txt"), b"old").unwrap();

        let prepared = manager
            .prepare_job(
                HostFileJobKind::Copy,
                vec![path_string(source_root.join("folder"))],
                Some(path_string(&target_root)),
            )
            .unwrap();
        assert!(prepared.conflicts[0].directory_replacement);
        assert!(
            manager
                .confirm_job(
                    &prepared.operation_id,
                    HostFileConflictPolicy::Replace,
                    false,
                )
                .is_err()
        );
        manager
            .confirm_job(
                &prepared.operation_id,
                HostFileConflictPolicy::Replace,
                true,
            )
            .unwrap();
        let completed = wait_for_job(&manager, &prepared.operation_id);
        assert_eq!(completed.state, HostFileJobState::Completed);
        assert_eq!(
            fs::read(target_root.join("folder/new.txt")).unwrap(),
            b"new"
        );
        assert!(!target_root.join("folder/old.txt").exists());
    }

    #[test]
    fn move_and_batch_delete_report_top_level_results() {
        let fixture = Fixture::new("move-delete");
        let manager = HostFileManager::new(&fixture.path).unwrap();
        let target = fixture.path.join("target");
        fs::create_dir_all(&target).unwrap();
        let moving = fixture.path.join("moving.txt");
        fs::write(&moving, b"moving").unwrap();
        let prepared = manager
            .prepare_job(
                HostFileJobKind::Move,
                vec![path_string(&moving)],
                Some(path_string(&target)),
            )
            .unwrap();
        manager
            .confirm_job(&prepared.operation_id, HostFileConflictPolicy::Skip, false)
            .unwrap();
        let moved = wait_for_job(&manager, &prepared.operation_id);
        assert_eq!(moved.state, HostFileJobState::Completed);
        assert_eq!(moved.processed_items, moved.stats.items);
        assert!(!moving.exists());
        assert_eq!(fs::read(target.join("moving.txt")).unwrap(), b"moving");

        let first = fixture.path.join("first.txt");
        let second = fixture.path.join("second");
        fs::write(&first, b"first").unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(second.join("nested.txt"), b"second").unwrap();
        let prepared = manager
            .prepare_job(
                HostFileJobKind::Delete,
                vec![path_string(&first), path_string(&second)],
                None,
            )
            .unwrap();
        manager
            .confirm_job(&prepared.operation_id, HostFileConflictPolicy::Skip, false)
            .unwrap();
        let deleted = wait_for_job(&manager, &prepared.operation_id);
        assert_eq!(deleted.state, HostFileJobState::Completed);
        assert_eq!(deleted.results.len(), 2);
        assert!(!first.exists());
        assert!(!second.exists());
    }

    #[test]
    fn server_generated_archive_contains_selected_descendants() {
        let fixture = Fixture::new("archive");
        let manager = HostFileManager::new(&fixture.path).unwrap();
        let folder = fixture.path.join("folder");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("nested.txt"), b"archive").unwrap();
        let download = manager.create_download(vec![path_string(&folder)]).unwrap();
        let ready = (0..200)
            .find_map(|_| {
                let snapshot = manager.download_status(&download.download_id).unwrap();
                if snapshot.state == "ready" {
                    Some(snapshot)
                } else {
                    thread::sleep(Duration::from_millis(10));
                    None
                }
            })
            .expect("archive did not become ready");
        assert!(ready.size_bytes.unwrap() > 0);
        let mut stream = manager.open_download(&download.download_id, None).unwrap();
        let mut bytes = Vec::new();
        stream.reader.read_to_end(&mut bytes).unwrap();
        let decoder = flate2::read::GzDecoder::new(bytes.as_slice());
        let mut archive = tar::Archive::new(decoder);
        let paths = archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path().unwrap().into_owned())
            .collect::<Vec<_>>();
        assert!(
            paths
                .iter()
                .any(|path| path == Path::new("folder/nested.txt"))
        );
    }

    #[test]
    fn filesystem_roots_are_rejected_before_planning() {
        let fixture = Fixture::new("root-source");
        let manager = HostFileManager::new(&fixture.path).unwrap();
        let canonical = fs::canonicalize(&fixture.path).unwrap();
        let root = canonical.ancestors().last().unwrap();
        let error = manager
            .prepare_job(HostFileJobKind::Delete, vec![path_string(root)], None)
            .unwrap_err();
        assert!(error.to_string().contains("Filesystem roots"));
    }

    #[test]
    fn upload_replace_commits_new_content_without_internal_residue() {
        let fixture = Fixture::new("upload-replace");
        let manager = HostFileManager::new(&fixture.path).unwrap();
        let target = fixture.path.join("payload.bin");
        fs::write(&target, b"old").unwrap();
        let conflict = manager
            .create_upload(&path_string(&fixture.path), "payload.bin", 3, None)
            .unwrap();
        assert!(conflict.requires_confirmation);
        let upload = manager
            .create_upload(
                &path_string(&fixture.path),
                "payload.bin",
                3,
                Some(HostFileConflictPolicy::Replace),
            )
            .unwrap()
            .upload
            .unwrap();
        manager
            .upload_chunk(&upload.upload_id, 0, &BASE64.encode(b"new"))
            .unwrap();
        let finished = manager.finish_upload(&upload.upload_id).unwrap();
        assert_eq!(finished.state, "completed");
        assert!(finished.error.is_none());
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert!(
            fs::read_dir(&fixture.path)
                .unwrap()
                .all(|entry| !is_internal_temp_name(&entry.unwrap().file_name().to_string_lossy()))
        );
    }

    #[test]
    fn empty_file_download_has_zero_content_length() {
        let fixture = Fixture::new("empty");
        let manager = HostFileManager::new(&fixture.path).unwrap();
        let empty = fixture.path.join("empty.txt");
        fs::write(&empty, []).unwrap();
        let download = manager.create_download(vec![path_string(&empty)]).unwrap();
        let mut stream = manager.open_download(&download.download_id, None).unwrap();
        let mut body = Vec::new();
        stream.reader.read_to_end(&mut body).unwrap();
        assert_eq!(stream.content_length, 0);
        assert!(body.is_empty());
    }
}
