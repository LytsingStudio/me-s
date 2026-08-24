use std::path::Path;

pub(crate) fn public_host_path(path: impl AsRef<Path>) -> String {
    let value = path.as_ref().to_string_lossy();
    #[cfg(windows)]
    {
        windows_public_path(&value)
    }
    #[cfg(not(windows))]
    {
        value.into_owned()
    }
}

#[cfg(any(windows, test))]
fn windows_public_path(value: &str) -> String {
    if let Some(path) = value.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{path}");
    }
    if let Some(path) = value.strip_prefix(r"\\?\") {
        let bytes = path.as_bytes();
        if bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/')
        {
            return path.to_owned();
        }
    }
    value.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_drive_and_unc_paths_use_public_forms() {
        assert_eq!(
            windows_public_path(r"\\?\C:\Users\xs\file.txt"),
            r"C:\Users\xs\file.txt"
        );
        assert_eq!(
            windows_public_path(r"\\?\UNC\server\share\folder"),
            r"\\server\share\folder"
        );
        assert_eq!(
            windows_public_path(r"C:\Users\xs\file.txt"),
            r"C:\Users\xs\file.txt"
        );
    }

    #[test]
    fn special_windows_namespaces_remain_opaque() {
        assert_eq!(
            windows_public_path(r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\folder"),
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\folder"
        );
        assert_eq!(
            windows_public_path(r"\\?\GLOBALROOT\Device\HarddiskVolume1"),
            r"\\?\GLOBALROOT\Device\HarddiskVolume1"
        );
    }
}
