use std::fs::OpenOptions;
use std::fs::Permissions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

use crate::error::AppError;

pub const OWNER_ONLY_FILE_MODE: u32 = 0o600;

pub fn read_existing_text(path: &Path) -> Result<String, AppError> {
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

pub fn append_to_private_file(path: &Path, text: &str) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file: std::fs::File = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(OWNER_ONLY_FILE_MODE)
        .open(path)?;
    file.write_all(text.as_bytes())?;

    std::fs::set_permissions(path, Permissions::from_mode(OWNER_ONLY_FILE_MODE))?;

    Ok(())
}

/// Replaces the file's contents atomically, leaving it readable only by its owner.
pub fn replace_private_file(path: &Path, text: &str) -> Result<(), AppError> {
    replace_checked_private_file(path, text, accept_written_file)
}

/// Like `replace_private_file`, but the replacement takes effect only if `check_written_file` accepts the written file.
pub fn replace_checked_private_file(
    path: &Path,
    text: &str,
    check_written_file: impl Fn(&Path) -> Result<(), AppError>,
) -> Result<(), AppError> {
    let target_path: PathBuf = get_symlink_resolved_path(path)?;
    let temporary_path: PathBuf = get_temporary_sibling_path(&target_path)?;

    let write_result: Result<(), AppError> =
        write_private_file(&temporary_path, text).and_then(|()| check_written_file(&temporary_path));

    if let Err(error) = write_result {
        std::fs::remove_file(&temporary_path).ok();

        return Err(error);
    }

    let rename_result: Result<(), std::io::Error> = std::fs::rename(&temporary_path, &target_path);

    if let Err(error) = rename_result {
        std::fs::remove_file(&temporary_path).ok();

        return Err(error.into());
    }

    Ok(())
}

fn accept_written_file(_written_path: &Path) -> Result<(), AppError> {
    Ok(())
}

/// A path that does not exist yet resolves to itself.
fn get_symlink_resolved_path(path: &Path) -> Result<PathBuf, AppError> {
    match std::fs::canonicalize(path) {
        Ok(resolved_path) => Ok(resolved_path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(error) => Err(error.into()),
    }
}

fn write_private_file(path: &Path, text: &str) -> Result<(), AppError> {
    let mut file: std::fs::File = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(OWNER_ONLY_FILE_MODE)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;

    std::fs::set_permissions(path, Permissions::from_mode(OWNER_ONLY_FILE_MODE))?;

    Ok(())
}

fn get_temporary_sibling_path(path: &Path) -> Result<PathBuf, AppError> {
    let file_name: &str = path
        .file_name()
        .and_then(|file_name| file_name.to_str())
        .ok_or_else(|| AppError::new(&format!("{} does not name a file", path.display())))?;

    Ok(path.with_file_name(format!(".{}.xost-{}.tmp", file_name, std::process::id())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_temp_directory(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("xost-private-file-test-{}-{}", std::process::id(), label))
    }

    fn get_mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn append_to_private_file_creates_the_file_readable_only_by_its_owner() {
        let directory: PathBuf = get_temp_directory("append");
        let path: PathBuf = directory.join("nested").join("config");

        append_to_private_file(&path, "[secrets]\n").unwrap();
        let mode: u32 = get_mode(&path);
        std::fs::remove_dir_all(&directory).ok();

        assert_eq!(mode, OWNER_ONLY_FILE_MODE);
    }

    #[test]
    fn replace_private_file_replaces_the_contents_and_restricts_the_mode_to_its_owner() {
        let directory: PathBuf = get_temp_directory("replace");
        std::fs::create_dir_all(&directory).unwrap();
        let path: PathBuf = directory.join("config");
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();

        replace_private_file(&path, "new\n").unwrap();
        let contents: String = std::fs::read_to_string(&path).unwrap();
        let mode: u32 = get_mode(&path);
        let directory_entry_count: usize = std::fs::read_dir(&directory).unwrap().count();
        std::fs::remove_dir_all(&directory).ok();

        assert_eq!(contents, "new\n");
        assert_eq!(mode, OWNER_ONLY_FILE_MODE);
        assert_eq!(directory_entry_count, 1);
    }

    #[test]
    fn replace_private_file_updates_the_target_of_a_symlink_and_keeps_the_link() {
        let directory: PathBuf = get_temp_directory("symlink");
        std::fs::create_dir_all(&directory).unwrap();
        let target_path: PathBuf = directory.join("real-config");
        let link_path: PathBuf = directory.join("config");
        std::fs::write(&target_path, "old\n").unwrap();
        std::os::unix::fs::symlink(&target_path, &link_path).unwrap();

        replace_private_file(&link_path, "new\n").unwrap();
        let link_is_symlink: bool = std::fs::symlink_metadata(&link_path).unwrap().file_type().is_symlink();
        let target_contents: String = std::fs::read_to_string(&target_path).unwrap();
        let directory_entry_count: usize = std::fs::read_dir(&directory).unwrap().count();
        std::fs::remove_dir_all(&directory).ok();

        assert!(link_is_symlink);
        assert_eq!(target_contents, "new\n");
        assert_eq!(directory_entry_count, 2);
    }

    #[test]
    fn replace_checked_private_file_leaves_the_file_and_no_temporary_file_when_the_check_fails() {
        let directory: PathBuf = get_temp_directory("rejected");
        std::fs::create_dir_all(&directory).unwrap();
        let path: PathBuf = directory.join("config");
        std::fs::write(&path, "old\n").unwrap();

        let result: Result<(), AppError> =
            replace_checked_private_file(&path, "new\n", |_written_path| Err(AppError::new("rejected")));
        let contents: String = std::fs::read_to_string(&path).unwrap();
        let directory_entry_count: usize = std::fs::read_dir(&directory).unwrap().count();
        std::fs::remove_dir_all(&directory).ok();

        assert!(result.is_err());
        assert_eq!(contents, "old\n");
        assert_eq!(directory_entry_count, 1);
    }

    #[test]
    fn replace_private_file_removes_the_temporary_file_when_the_rename_fails() {
        let directory: PathBuf = get_temp_directory("rename-fails");
        let occupied_path: PathBuf = directory.join("config");
        std::fs::create_dir_all(occupied_path.join("child")).unwrap();

        let result: Result<(), AppError> = replace_private_file(&occupied_path, "new\n");
        let directory_entry_count: usize = std::fs::read_dir(&directory).unwrap().count();
        std::fs::remove_dir_all(&directory).ok();

        assert!(result.is_err());
        assert_eq!(directory_entry_count, 1);
    }
}
