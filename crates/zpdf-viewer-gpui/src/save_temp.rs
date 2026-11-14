use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_TEMP_ATTEMPTS: usize = 64;
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// A fresh sibling file owned by one save attempt. Failed saves close and
/// remove it; a successful commit atomically replaces the destination.
///
/// Exclusive creation, rather than secrecy of the name, prevents following or
/// truncating a pre-existing entry. The containing directory must still protect
/// created entries from removal or replacement by other principals.
pub(crate) struct SaveTemp {
    file: Option<File>,
    path: PathBuf,
    destination: PathBuf,
    committed: bool,
}

impl SaveTemp {
    pub(crate) fn new(destination: &Path) -> io::Result<Self> {
        if destination.file_name().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "save destination has no file name",
            ));
        }
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        for _ in 0..MAX_TEMP_ATTEMPTS {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(".zpdf-save-{}-{id}.tmp", std::process::id()));
            match create_private_file(&path) {
                Ok(file) => {
                    return Ok(Self {
                        file: Some(file),
                        path,
                        destination: destination.to_owned(),
                        committed: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a fresh temporary save file",
        ))
    }

    pub(crate) fn file_mut(&mut self) -> &mut File {
        self.file
            .as_mut()
            .expect("save file remains open until commit")
    }

    /// The caller must finish writing and sync the file before committing.
    pub(crate) fn commit(mut self) -> io::Result<()> {
        // Close before rename (and before error cleanup), including on Windows.
        drop(self.file.take());
        fs::rename(&self.path, &self.destination)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for SaveTemp {
    fn drop(&mut self) {
        drop(self.file.take());
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn create_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            for _ in 0..MAX_TEMP_ATTEMPTS {
                let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir()
                    .join(format!("zpdf-save-test-{}-{id}", std::process::id()));
                match builder.create(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("could not create test directory: {error}"),
                }
            }
            panic!("could not create a fresh test directory");
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn preexisting_temporary_file_is_preserved() {
        let dir = TestDir::new();
        let destination = dir.0.join("document.pdf");
        let legacy_temp = destination.with_extension("pdf.tmp");
        fs::write(&legacy_temp, b"unrelated file").unwrap();

        let error = create_private_file(&legacy_temp).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        let save = SaveTemp::new(&destination).unwrap();
        assert_ne!(save.path, legacy_temp);
        drop(save);
        assert_eq!(fs::read(&legacy_temp).unwrap(), b"unrelated file");
    }

    #[test]
    fn aborted_save_cleans_up_without_changing_the_destination() {
        let dir = TestDir::new();
        let destination = dir.0.join("document.pdf");
        fs::write(&destination, b"original").unwrap();
        let temp_path;
        {
            let mut save = SaveTemp::new(&destination).unwrap();
            temp_path = save.path.clone();
            save.file_mut().write_all(b"partial output").unwrap();
        }
        assert!(!temp_path.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"original");
    }

    #[test]
    fn commit_replaces_the_destination_using_a_sibling() {
        let dir = TestDir::new();
        let destination = dir.0.join("document.pdf");
        fs::write(&destination, b"original").unwrap();
        let mut save = SaveTemp::new(&destination).unwrap();
        let temp_path = save.path.clone();
        assert_eq!(temp_path.parent(), destination.parent());
        save.file_mut().write_all(b"updated").unwrap();
        save.file_mut().sync_all().unwrap();
        save.commit().unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"updated");
        assert!(!temp_path.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&destination).unwrap().permissions().mode() & 0o077,
                0
            );
        }
    }

    #[test]
    fn failed_commit_cleans_up_the_temporary_file() {
        let dir = TestDir::new();
        let destination = dir.0.join("existing-directory");
        fs::create_dir(&destination).unwrap();
        let save = SaveTemp::new(&destination).unwrap();
        let temp_path = save.path.clone();
        assert!(save.commit().is_err());
        assert!(destination.is_dir());
        assert!(!temp_path.exists());
    }
}
