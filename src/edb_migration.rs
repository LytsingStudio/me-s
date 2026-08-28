use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::Result;

#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

pub(crate) const COMPATIBILITY_BASELINE_VERSION: u8 = 29;
pub(crate) const CURRENT_FILE_VERSION: u8 = 41;
pub(crate) const CURRENT_FILE_MAGIC: [u8; 8] = *b"MEDB\x29\0\0\0";
const FILE_SIGNATURE: &[u8; 4] = b"MEDB";
const FILE_MAGIC_SIZE: usize = 8;
const FILE_HEADER_SIZE: usize = 16;

type MigrationFn = fn(Vec<u8>) -> Result<Vec<u8>>;

#[derive(Clone, Copy)]
struct MigrationStep {
    from: u8,
    to: u8,
    migrate: MigrationFn,
}

const MIGRATION_STEPS: &[MigrationStep] = &[
    MigrationStep {
        from: 29,
        to: 30,
        migrate: migrate_v29_to_v30,
    },
    MigrationStep {
        from: 30,
        to: 31,
        migrate: migrate_v30_to_v31,
    },
    MigrationStep {
        from: 31,
        to: 32,
        migrate: migrate_v31_to_v32,
    },
    MigrationStep {
        from: 32,
        to: 33,
        migrate: migrate_v32_to_v33,
    },
    MigrationStep {
        from: 33,
        to: 34,
        migrate: migrate_v33_to_v34,
    },
    MigrationStep {
        from: 34,
        to: 35,
        migrate: migrate_v34_to_v35,
    },
    MigrationStep {
        from: 35,
        to: 36,
        migrate: migrate_v35_to_v36,
    },
    MigrationStep {
        from: 36,
        to: 37,
        migrate: migrate_v36_to_v37,
    },
    MigrationStep {
        from: 37,
        to: 38,
        migrate: migrate_v37_to_v38,
    },
    MigrationStep {
        from: 38,
        to: 39,
        migrate: migrate_v38_to_v39,
    },
    MigrationStep {
        from: 39,
        to: 40,
        migrate: migrate_v39_to_v40,
    },
    MigrationStep {
        from: 40,
        to: 41,
        migrate: migrate_v40_to_v41,
    },
];

#[derive(Debug)]
pub(crate) struct MigrationPlan {
    pub source_version: u8,
    pub target_version: u8,
    pub bytes: Vec<u8>,
}

pub(crate) fn plan(bytes: &[u8]) -> Result<Option<MigrationPlan>> {
    validate_header(bytes)?;
    let source_version = bytes[FILE_SIGNATURE.len()];
    if source_version < COMPATIBILITY_BASELINE_VERSION {
        return Err(format!(
            "EDB version {source_version} predates the supported migration baseline v{COMPATIBILITY_BASELINE_VERSION}"
        )
        .into());
    }
    if source_version > CURRENT_FILE_VERSION {
        return Err(format!(
            "EDB version {source_version} is newer than this me build supports (v{CURRENT_FILE_VERSION})"
        )
        .into());
    }
    if source_version == CURRENT_FILE_VERSION {
        return Ok(None);
    }

    let mut migrated = bytes.to_vec();
    let mut version = source_version;
    while version < CURRENT_FILE_VERSION {
        let step = MIGRATION_STEPS
            .iter()
            .find(|step| step.from == version)
            .ok_or_else(|| {
                format!(
                    "EDB migration chain has no v{version} to v{} step",
                    version + 1
                )
            })?;
        if step.to != version + 1 {
            return Err(format!(
                "EDB migration step v{} to v{} is not adjacent",
                step.from, step.to
            )
            .into());
        }
        migrated = (step.migrate)(migrated)?;
        version = step.to;
        validate_header_for_version(&migrated, version)?;
    }

    Ok(Some(MigrationPlan {
        source_version,
        target_version: version,
        bytes: migrated,
    }))
}

pub(crate) fn commit(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or("EDB path has no parent")?;
    let temp_path = migration_temp_path(path)?;
    let result = (|| -> Result<()> {
        let mut replacement = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)?;
        #[cfg(unix)]
        fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o600))?;
        replacement.write_all(bytes)?;
        replacement.sync_all()?;
        drop(replacement);
        replace_file(&temp_path, path)?;
        let _ = sync_parent_directory(parent);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn validate_header(bytes: &[u8]) -> Result<()> {
    if bytes.len() < FILE_HEADER_SIZE {
        return Err(format!(
            "EDB header is incomplete: expected at least {FILE_HEADER_SIZE} bytes, found {}",
            bytes.len()
        )
        .into());
    }
    if bytes.get(..FILE_SIGNATURE.len()) != Some(FILE_SIGNATURE) {
        return Err("unsupported or corrupt EDB signature".into());
    }
    if bytes[5..FILE_MAGIC_SIZE] != [0, 0, 0] {
        return Err("unsupported or corrupt EDB header flags".into());
    }
    Ok(())
}

fn validate_header_for_version(bytes: &[u8], version: u8) -> Result<()> {
    validate_header(bytes)?;
    if bytes[FILE_SIGNATURE.len()] != version {
        return Err(format!("EDB migration did not produce v{version}").into());
    }
    Ok(())
}

fn migrate_v29_to_v30(mut bytes: Vec<u8>) -> Result<Vec<u8>> {
    validate_header_for_version(&bytes, 29)?;
    bytes[FILE_SIGNATURE.len()] = 30;
    Ok(bytes)
}

fn migrate_v30_to_v31(mut bytes: Vec<u8>) -> Result<Vec<u8>> {
    validate_header_for_version(&bytes, 30)?;
    bytes[FILE_SIGNATURE.len()] = 31;
    Ok(bytes)
}

fn migrate_v31_to_v32(mut bytes: Vec<u8>) -> Result<Vec<u8>> {
    validate_header_for_version(&bytes, 31)?;
    bytes[FILE_SIGNATURE.len()] = 32;
    Ok(bytes)
}

fn migrate_v32_to_v33(bytes: Vec<u8>) -> Result<Vec<u8>> {
    validate_header_for_version(&bytes, 32)?;
    crate::event::migrate_prompt_sources_v32_to_v33(bytes)
}

fn migrate_v33_to_v34(mut bytes: Vec<u8>) -> Result<Vec<u8>> {
    validate_header_for_version(&bytes, 33)?;
    bytes[FILE_SIGNATURE.len()] = 34;
    Ok(bytes)
}

fn migrate_v34_to_v35(bytes: Vec<u8>) -> Result<Vec<u8>> {
    validate_header_for_version(&bytes, 34)?;
    crate::event::migrate_agent_orchestrator_v34_to_v35(bytes)
}

fn migrate_v35_to_v36(bytes: Vec<u8>) -> Result<Vec<u8>> {
    validate_header_for_version(&bytes, 35)?;
    crate::event::migrate_compact_kind_v35_to_v36(bytes)
}

fn migrate_v36_to_v37(bytes: Vec<u8>) -> Result<Vec<u8>> {
    validate_header_for_version(&bytes, 36)?;
    crate::event::migrate_compact_strategy_v36_to_v37(bytes)
}

fn migrate_v37_to_v38(bytes: Vec<u8>) -> Result<Vec<u8>> {
    validate_header_for_version(&bytes, 37)?;
    crate::event::migrate_compact_stage_count_v37_to_v38(bytes)
}

fn migrate_v38_to_v39(mut bytes: Vec<u8>) -> Result<Vec<u8>> {
    validate_header_for_version(&bytes, 38)?;
    bytes[FILE_SIGNATURE.len()] = 39;
    Ok(bytes)
}

fn migrate_v39_to_v40(mut bytes: Vec<u8>) -> Result<Vec<u8>> {
    validate_header_for_version(&bytes, 39)?;
    bytes[FILE_SIGNATURE.len()] = 40;
    Ok(bytes)
}

fn migrate_v40_to_v41(bytes: Vec<u8>) -> Result<Vec<u8>> {
    validate_header_for_version(&bytes, 40)?;
    crate::event::migrate_edb_id_v40_to_v41(bytes)
}

fn migration_temp_path(path: &Path) -> Result<PathBuf> {
    let parent = path.parent().ok_or("EDB path has no parent")?;
    let file_name = path
        .file_name()
        .ok_or("EDB path has no file name")?
        .to_string_lossy();
    Ok(parent.join(format!(
        ".{file_name}.migrate-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    )))
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)?;
    Ok(())
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let replaced = unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            source.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if replaced == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> Result<()> {
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(version: u8) -> Vec<u8> {
        let mut bytes = vec![0_u8; FILE_HEADER_SIZE];
        bytes[..FILE_SIGNATURE.len()].copy_from_slice(FILE_SIGNATURE);
        bytes[FILE_SIGNATURE.len()] = version;
        bytes
    }

    #[test]
    fn migration_registry_is_an_adjacent_chain_from_v29_to_current() {
        let mut expected = COMPATIBILITY_BASELINE_VERSION;
        for step in MIGRATION_STEPS {
            assert_eq!(step.from, expected);
            assert_eq!(step.to, expected + 1);
            expected = step.to;
        }
        assert_eq!(expected, CURRENT_FILE_VERSION);
    }

    #[test]
    fn v29_migrates_through_every_step_to_current() {
        let mut source = header(29);
        source[8..16].copy_from_slice(&42_u64.to_le_bytes());
        let planned = plan(&source).unwrap().unwrap();
        assert_eq!(planned.source_version, 29);
        assert_eq!(planned.target_version, 41);
        assert_eq!(planned.bytes[4], 41);
        assert!(planned.bytes.len() > source.len());
        assert!(plan(&planned.bytes).unwrap().is_none());
    }

    #[test]
    fn v30_migrates_through_v31_to_current() {
        let mut source = header(30);
        source[8..16].copy_from_slice(&77_u64.to_le_bytes());
        let planned = plan(&source).unwrap().unwrap();
        assert_eq!(planned.source_version, 30);
        assert_eq!(planned.target_version, 41);
        assert_eq!(planned.bytes[4], 41);
        assert!(planned.bytes.len() > source.len());
    }

    #[test]
    fn v31_migrates_through_v32_to_current() {
        let mut source = header(31);
        source[8..16].copy_from_slice(&91_u64.to_le_bytes());
        let planned = plan(&source).unwrap().unwrap();
        assert_eq!(planned.source_version, 31);
        assert_eq!(planned.target_version, 41);
        assert_eq!(planned.bytes[4], 41);
        assert!(planned.bytes.len() > source.len());
    }

    #[test]
    fn versions_outside_the_supported_range_are_not_planned() {
        assert!(
            plan(&header(28))
                .unwrap_err()
                .to_string()
                .contains("baseline")
        );
        let v38 = plan(&header(38)).unwrap().unwrap();
        assert_eq!(v38.source_version, 38);
        assert_eq!(v38.target_version, 41);
        assert_eq!(v38.bytes[4], 41);
        let v39 = plan(&header(39)).unwrap().unwrap();
        assert_eq!(v39.source_version, 39);
        assert_eq!(v39.target_version, 41);
        assert_eq!(v39.bytes[4], 41);
        let v40 = plan(&header(40)).unwrap().unwrap();
        assert_eq!(v40.target_version, 41);
        assert!(plan(&header(41)).unwrap().is_none());
        assert!(plan(&header(42)).unwrap_err().to_string().contains("newer"));
        assert!(plan(b"MEDB").is_err());
    }
}
