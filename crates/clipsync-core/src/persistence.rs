//! Persistência atômica canônica para os arquivos de estado do core.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Substitui `path` atomicamente por `contents` e sincroniza dados e diretório.
pub(crate) fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    atomic_write_with_mode(path, contents, None)
}

pub(crate) fn atomic_write_with_mode(
    path: &Path,
    contents: &[u8],
    mode: Option<u32>,
) -> io::Result<()> {
    #[cfg(not(unix))]
    let _ = mode;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let mut temporary = TemporaryPath::create(path)?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .file
            .set_permissions(fs::Permissions::from_mode(mode))?;
    }
    temporary.file.write_all(contents)?;
    temporary.file.sync_all()?;
    fs::rename(&temporary.path, path)?;
    temporary.committed = true;

    fs::File::open(parent)?.sync_all()
}

struct TemporaryPath {
    path: PathBuf,
    file: fs::File,
    committed: bool,
}

impl TemporaryPath {
    fn create(destination: &Path) -> io::Result<Self> {
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        let name = destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("clipsync");

        for _ in 0..16 {
            let path = parent.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file,
                        committed: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }

        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "não foi possível reservar arquivo temporário",
        ))
    }
}

impl Drop for TemporaryPath {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "clipsync-atomic-{name}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn atomically_replaces_existing_file() {
        let dir = temp_dir("replace");
        let path = dir.join("state.toml");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, b"old").unwrap();

        atomic_write(&path, b"new").unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_commit_preserves_destination() {
        let dir = temp_dir("failure");
        let destination = dir.join("state.toml");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&destination, b"old").unwrap();

        assert!(atomic_write(&destination.join("child"), b"new").is_err());

        assert_eq!(fs::read(&destination).unwrap(), b"old");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }
}
