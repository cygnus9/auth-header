#[cfg(unix)]
use std::fs;
use std::{fs::File, path::Path};

#[cfg(unix)]
pub(crate) fn set_private_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("could not secure {}: {error}", path.display()))
}

#[cfg(not(unix))]
pub(crate) fn set_private_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
pub(crate) fn set_private_file(file: &File) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("could not secure failure record: {error}"))
}

#[cfg(not(unix))]
pub(crate) fn set_private_file(_file: &File) -> Result<(), String> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::{NamedTempFile, tempdir};

    use super::*;

    #[test]
    fn set_private_directory_uses_restrictive_mode() {
        let directory = tempdir().expect("create temporary directory");

        set_private_directory(directory.path()).expect("secure temporary directory");

        assert_eq!(
            directory.path().metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn set_private_file_uses_restrictive_mode() {
        let file = NamedTempFile::new().expect("create temporary file");

        set_private_file(file.as_file()).expect("secure temporary file");

        assert_eq!(
            file.as_file().metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
