//! Small filesystem and hashing helpers shared by setup, service management
//! and updates.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Lowercase hex SHA-256 of a byte slice.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// Lowercase hex SHA-256 of a file, streamed so large artifacts stay cheap.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

/// The first eight hex digits of the SHA-256 of `text`.
///
/// Stable across builds and platforms (unlike `DefaultHasher`), so it can be
/// persisted in session keys and service names.
pub fn short_hash(text: &str) -> String {
    sha256_hex(text.as_bytes())[..8].to_string()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Normalise a path for hashing: forward slashes, no trailing separator, and
/// lowercase on platforms whose default filesystem is case-insensitive.
pub fn normalize_path(path: &Path) -> String {
    let mut text = path.to_string_lossy().replace('\\', "/");
    while text.len() > 1 && text.ends_with('/') {
        text.pop();
    }
    if cfg!(any(windows, target_os = "macos")) {
        text = text.to_lowercase();
    }
    text
}

/// Write a file atomically: write a sibling temporary file, then rename it
/// over the target. A crash never leaves a half-written configuration.
pub fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let temp = path.with_file_name(format!(".{file_name}.reason-network-tmp"));
    std::fs::write(&temp, contents)?;
    match std::fs::rename(&temp, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            Err(error)
        }
    }
}

/// Copy `path` to a timestamped sibling backup and return the backup path.
///
/// The backup name never collides with an earlier one, so repeated runs keep
/// every previous state.
pub fn backup_file(path: &Path) -> std::io::Result<PathBuf> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let mut candidate = path.with_file_name(format!("{file_name}.reason-network-backup-{stamp}"));
    let mut counter = 1;
    while candidate.exists() {
        candidate = path.with_file_name(format!(
            "{file_name}.reason-network-backup-{stamp}-{counter}"
        ));
        counter += 1;
    }
    std::fs::copy(path, &candidate)?;
    Ok(candidate)
}

/// Find an executable by name in a list of directories.
///
/// On Windows the usual launcher extensions are tried as well, because npm
/// and other installers create `.cmd` shims rather than `.exe` files.
pub fn find_executable(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let candidates: Vec<String> = if cfg!(windows) {
        ["exe", "cmd", "bat", "com"]
            .iter()
            .map(|extension| format!("{name}.{extension}"))
            .chain(std::iter::once(name.to_string()))
            .collect()
    } else {
        vec![name.to_string()]
    };
    for dir in dirs {
        for candidate in &candidates {
            let path = dir.join(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

/// The directories listed in `PATH`.
pub fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|value| std::env::split_paths(&value).collect())
        .unwrap_or_default()
}

/// The current user's home directory.
pub fn home_dir() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn short_hash_is_stable_and_short() {
        assert_eq!(short_hash("abc"), "ba7816bf");
    }

    #[test]
    fn backups_never_overwrite_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        std::fs::write(&file, "one").unwrap();
        let first = backup_file(&file).unwrap();
        std::fs::write(&file, "two").unwrap();
        let second = backup_file(&file).unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read_to_string(first).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(second).unwrap(), "two");
    }

    #[test]
    fn atomic_write_replaces_contents_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested").join("config.toml");
        atomic_write(&file, b"a = 1").unwrap();
        atomic_write(&file, b"a = 2").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "a = 2");
        let entries: Vec<_> = std::fs::read_dir(file.parent().unwrap()).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }
}
