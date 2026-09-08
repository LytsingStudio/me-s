use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::Path,
};

use crate::Result;

pub(super) const PROGRAMS: [&str; 3] = ["me-s.exe", "me-gateway.exe", "me-client.exe"];
const MAX_PROGRAM_BYTES: u64 = 1024 * 1024 * 1024;

pub(super) fn extract(package: &Path, destination: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(File::open(package)?)?;
    if archive.len() != PROGRAMS.len() {
        return Err("the portable package must contain exactly the three ME programs".into());
    }
    let mut names = HashSet::new();
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        if !PROGRAMS.contains(&entry.name())
            || !names.insert(entry.name().to_owned())
            || !entry.is_file()
            || entry.is_symlink()
            || entry.size() == 0
            || entry.size() > MAX_PROGRAM_BYTES
        {
            return Err(format!("invalid portable package member: {}", entry.name()).into());
        }
    }
    fs::create_dir(destination)?;
    for name in PROGRAMS {
        let mut entry = archive.by_name(name)?;
        let size = entry.size();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination.join(name))?;
        let copied = std::io::copy(&mut entry.by_ref().take(size + 1), &mut file)?;
        if copied != size {
            return Err(format!("incomplete portable program: {name}").into());
        }
        file.sync_all()?;
        validate_program(&destination.join(name))?;
    }
    Ok(())
}

fn validate_program(path: &Path) -> Result<()> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut dos = [0; 64];
    file.read_exact(&mut dos)?;
    let offset = u32::from_le_bytes(dos[60..64].try_into()?) as u64;
    if &dos[..2] != b"MZ" || offset < 64 || offset + 26 > size {
        return Err(format!("invalid Windows program: {}", path.display()).into());
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut header = [0; 26];
    file.read_exact(&mut header)?;
    if &header[..4] != b"PE\0\0"
        || u16::from_le_bytes([header[4], header[5]]) != 0x8664
        || u16::from_le_bytes([header[24], header[25]]) != 0x20b
        || u16::from_le_bytes([header[22], header[23]]) & 0x2000 != 0
    {
        return Err(format!(
            "portable program is not a Windows x64 executable: {}",
            path.display()
        )
        .into());
    }
    Ok(())
}

fn regular_file(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(format!(
            "refusing to replace a non-regular program: {}",
            path.display()
        )
        .into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn replace_product(
    install: &Path,
    staged: &Path,
    backup: &Path,
    verify: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    replace_product_with(
        install,
        staged,
        backup,
        |from, to| fs::rename(from, to),
        verify,
    )
}

fn replace_product_with(
    install: &Path,
    staged: &Path,
    backup: &Path,
    mut rename: impl FnMut(&Path, &Path) -> std::io::Result<()>,
    verify: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    for name in PROGRAMS {
        if !regular_file(&staged.join(name))? || fs::metadata(staged.join(name))?.len() == 0 {
            return Err(format!("missing staged program: {name}").into());
        }
        regular_file(&install.join(name))?;
    }
    fs::create_dir(backup)?;
    let mut backed_up = Vec::new();
    let mut installed = Vec::new();
    let result = (|| -> Result<()> {
        for name in PROGRAMS {
            if install.join(name).try_exists()? {
                rename(&install.join(name), &backup.join(name))?;
                backed_up.push(name);
            }
        }
        for name in PROGRAMS {
            rename(&staged.join(name), &install.join(name))?;
            installed.push(name);
        }
        verify(install)
    })();
    if let Err(error) = result {
        let mut failures = Vec::new();
        for name in installed.into_iter().rev() {
            if let Err(error) = fs::remove_file(install.join(name)) {
                failures.push(format!("cannot remove the new {name}: {error}"));
            }
        }
        for name in backed_up.into_iter().rev() {
            let restored = (|| -> std::io::Result<()> {
                if install.join(name).try_exists()? {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "destination is still occupied",
                    ));
                }
                rename(&backup.join(name), &install.join(name))
            })();
            if let Err(error) = restored {
                failures.push(format!("cannot restore {name}: {error}"));
            }
        }
        if failures.is_empty() {
            return Err(
                format!("update failed; the original programs were restored: {error}").into(),
            );
        }
        return Err(format!(
            "update failed: {error}; recovery is incomplete: {}; backups retained at {}",
            failures.join("; "),
            backup.display()
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, path::PathBuf};

    fn directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "me-portable-test-{}",
            super::super::random_suffix().unwrap()
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    fn executable() -> Vec<u8> {
        let mut bytes = vec![0; 90];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[60..64].copy_from_slice(&64u32.to_le_bytes());
        bytes[64..68].copy_from_slice(b"PE\0\0");
        bytes[68..70].copy_from_slice(&0x8664u16.to_le_bytes());
        bytes[88..90].copy_from_slice(&0x20bu16.to_le_bytes());
        bytes
    }

    fn package(path: &Path, names: &[&str], bytes: &[u8]) {
        let mut writer = zip::ZipWriter::new(File::create(path).unwrap());
        for name in names {
            writer
                .start_file(
                    *name,
                    zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Deflated),
                )
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn archive_requires_the_exact_complete_product_and_valid_machine_type() {
        let root = directory();
        let zip = root.join("portable.zip");
        let target = root.join("new");
        package(&zip, &PROGRAMS, &executable());
        extract(&zip, &target).unwrap();
        for name in PROGRAMS {
            assert_eq!(fs::read(target.join(name)).unwrap(), executable());
        }
        for names in [
            vec!["me-s.exe"],
            vec!["../me-s.exe", "me-gateway.exe", "me-client.exe"],
            vec!["me-s.exe", "me-gateway.exe", "ME-client.exe"],
        ] {
            package(&zip, &names, &executable());
            assert!(extract(&zip, &root.join("invalid")).is_err());
            assert!(!root.join("invalid").exists());
        }
        package(&zip, &PROGRAMS, b"not executable");
        assert!(extract(&zip, &root.join("invalid")).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn replacement_and_failed_verification_preserve_unrelated_data() {
        for fail in [false, true] {
            let root = directory();
            let staged = root.join("new");
            fs::create_dir(&staged).unwrap();
            for name in PROGRAMS {
                fs::write(root.join(name), format!("old {name}")).unwrap();
                fs::write(staged.join(name), format!("new {name}")).unwrap();
            }
            fs::write(root.join("me-client.sqlite3"), b"user data").unwrap();
            fs::write(root.join("Uninstall ME.exe"), b"uninstaller").unwrap();
            let result = replace_product(&root, &staged, &root.join("backup"), |installed| {
                for name in PROGRAMS {
                    assert_eq!(
                        fs::read_to_string(installed.join(name)).unwrap(),
                        format!("new {name}")
                    );
                }
                if fail {
                    Err("version verification failed".into())
                } else {
                    Ok(())
                }
            });
            assert_eq!(result.is_err(), fail);
            for name in PROGRAMS {
                assert_eq!(
                    fs::read_to_string(root.join(name)).unwrap(),
                    format!("{} {name}", if fail { "old" } else { "new" })
                );
            }
            assert_eq!(
                fs::read(root.join("me-client.sqlite3")).unwrap(),
                b"user data"
            );
            assert_eq!(
                fs::read(root.join("Uninstall ME.exe")).unwrap(),
                b"uninstaller"
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn partial_installation_is_restored_to_its_original_shape_on_failure() {
        let root = directory();
        let staged = root.join("new");
        fs::create_dir(&staged).unwrap();
        fs::write(root.join(PROGRAMS[0]), b"old").unwrap();
        for name in PROGRAMS {
            fs::write(staged.join(name), b"new").unwrap();
        }
        assert!(
            replace_product(&root, &staged, &root.join("backup"), |_| Err(
                "failure".into()
            ))
            .is_err()
        );
        assert_eq!(fs::read(root.join(PROGRAMS[0])).unwrap(), b"old");
        assert!(!root.join(PROGRAMS[1]).exists());
        assert!(!root.join(PROGRAMS[2]).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn every_backup_or_install_rename_failure_restores_all_originals() {
        for fail_at in 0..6 {
            let root = directory();
            let staged = root.join("new");
            fs::create_dir(&staged).unwrap();
            for name in PROGRAMS {
                fs::write(root.join(name), b"old").unwrap();
                fs::write(staged.join(name), b"new").unwrap();
            }
            let mut operation = 0;
            let result = replace_product_with(
                &root,
                &staged,
                &root.join("backup"),
                |from, to| {
                    let current = operation;
                    operation += 1;
                    if current == fail_at {
                        return Err(std::io::Error::other("injected rename failure"));
                    }
                    fs::rename(from, to)
                },
                |_| panic!("verification must not run after an install failure"),
            );
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("original programs were restored")
            );
            for name in PROGRAMS {
                assert_eq!(fs::read(root.join(name)).unwrap(), b"old");
            }
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn failed_recovery_retains_the_unrestored_backup_and_reports_its_path() {
        let root = directory();
        let staged = root.join("new");
        fs::create_dir(&staged).unwrap();
        let backup = root.join("backup");
        for name in PROGRAMS {
            fs::write(root.join(name), b"old").unwrap();
            fs::write(staged.join(name), b"new").unwrap();
        }
        let mut operation = 0;
        let error = replace_product_with(
            &root,
            &staged,
            &backup,
            |from, to| {
                let current = operation;
                operation += 1;
                if [3, 4].contains(&current) {
                    return Err(std::io::Error::other("injected failure"));
                }
                fs::rename(from, to)
            },
            |_| Ok(()),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("recovery is incomplete"));
        assert!(error.contains(backup.to_str().unwrap()));
        assert_eq!(fs::read(backup.join("me-client.exe")).unwrap(), b"old");
        for name in ["me-s.exe", "me-gateway.exe"] {
            assert_eq!(fs::read(root.join(name)).unwrap(), b"old");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn wrong_machine_and_crc_failure_are_rejected_before_replacement() {
        let root = directory();
        let archive = root.join("portable.zip");
        let mut wrong = executable();
        wrong[68..70].copy_from_slice(&0xaa64u16.to_le_bytes());
        package(&archive, &PROGRAMS, &wrong);
        assert!(extract(&archive, &root.join("wrong")).is_err());
        package(&archive, &PROGRAMS, &executable());
        let mut bytes = fs::read(&archive).unwrap();
        let central = bytes
            .windows(4)
            .position(|chunk| chunk == b"PK\x01\x02")
            .unwrap();
        bytes[central + 16] ^= 1;
        fs::write(&archive, bytes).unwrap();
        assert!(extract(&archive, &root.join("corrupt")).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn an_empty_old_program_can_be_repaired() {
        let root = directory();
        let staged = root.join("new");
        fs::create_dir(&staged).unwrap();
        for name in PROGRAMS {
            fs::write(root.join(name), b"").unwrap();
            fs::write(staged.join(name), b"new").unwrap();
        }
        replace_product(&root, &staged, &root.join("backup"), |_| Ok(())).unwrap();
        for name in PROGRAMS {
            assert_eq!(fs::read(root.join(name)).unwrap(), b"new");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn archive_symlinks_are_rejected_without_creating_an_extraction_directory() {
        let root = directory();
        let archive = root.join("portable.zip");
        let mut writer = zip::ZipWriter::new(File::create(&archive).unwrap());
        writer
            .add_symlink(
                PROGRAMS[0],
                "outside.exe",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        for name in &PROGRAMS[1..] {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(&executable()).unwrap();
        }
        writer.finish().unwrap();
        assert!(extract(&archive, &root.join("new")).is_err());
        assert!(!root.join("new").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn replacement_refuses_symlink_targets_and_preserves_their_contents() {
        let root = directory();
        let staged = root.join("new");
        fs::create_dir(&staged).unwrap();
        for name in PROGRAMS {
            fs::write(staged.join(name), b"new").unwrap();
        }
        let unrelated = root.join("unrelated");
        fs::write(&unrelated, b"keep").unwrap();
        std::os::unix::fs::symlink(&unrelated, root.join(PROGRAMS[0])).unwrap();
        assert!(replace_product(&root, &staged, &root.join("backup"), |_| Ok(())).is_err());
        assert_eq!(fs::read(unrelated).unwrap(), b"keep");
        assert!(!root.join("backup").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
