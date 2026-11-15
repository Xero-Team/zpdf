use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_TEMP_ATTEMPTS: usize = 64;
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[cfg(all(test, any(target_os = "macos", target_os = "freebsd")))]
#[path = "save_temp_acl_tests.rs"]
mod native_acl_tests;

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
        // Keep incomplete output private, then restore the destination's whole
        // access policy before rename. Any failure leaves the original intact.
        #[cfg(unix)]
        preserve_destination_access(
            self.file.as_ref().expect("save file is open"),
            &self.destination,
        )?;
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
    let file = options.open(path)?;
    #[cfg(unix)]
    if let Err(error) = access_acl::prepare_private(&file) {
        // No document data has been written. Closing/removing this empty file
        // also handles Darwin ACLs that cannot be made private with mode0600.
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(file)
}

/// Install the original file's access policy before publishing the replacement.
/// A mode is meaningful only together with its owner, group and access ACL.
#[cfg(unix)]
fn preserve_destination_access(file: &File, destination: &Path) -> io::Result<()> {
    use std::os::unix::fs::{fchown, MetadataExt, OpenOptionsExt};

    let initial = match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.is_file() => metadata,
        // A symlink is an entry being replaced, not an ACL/ownership source.
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    // Bind metadata and ACL reads to one regular inode. O_NONBLOCK prevents a
    // concurrent replacement with a FIFO from blocking this save.
    let original = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(destination)?;
    let metadata = original.metadata()?;
    if !metadata.is_file() || metadata.dev() != initial.dev() || metadata.ino() != initial.ino() {
        return Err(io::Error::other(
            "save destination changed while reading its access policy",
        ));
    }
    let acl = access_acl::AccessAcl::read(&original)?;
    let current = file.metadata()?;
    let uid = (current.uid() != metadata.uid()).then_some(metadata.uid());
    let gid = (current.gid() != metadata.gid()).then_some(metadata.gid());
    if uid.is_some() || gid.is_some() {
        // In particular, an unprivileged user may preserve a supplementary
        // group but must fail rather than replace a file owned by another user.
        fchown(file, uid, gid)?;
    }
    let ownership = file.metadata()?;
    if ownership.uid() != metadata.uid() || ownership.gid() != metadata.gid() {
        return Err(io::Error::other(
            "could not preserve save destination ownership",
        ));
    }
    // Replacing the inherited ACL must precede chmod: on Linux the mode's
    // group bits are the ACL mask and could otherwise reactivate named grants.
    acl.apply(file)?;
    file.set_permissions(metadata.permissions())?;
    acl.verify(file)?;
    let actual = file.metadata()?;
    if actual.uid() != metadata.uid()
        || actual.gid() != metadata.gid()
        || actual.mode() & 0o7777 != metadata.mode() & 0o7777
    {
        return Err(io::Error::other(
            "could not preserve save destination access policy",
        ));
    }
    file.sync_all()
}

#[cfg(target_os = "linux")]
mod access_acl {
    use super::*;
    use std::os::fd::AsRawFd;

    const ACL_NAME: &std::ffi::CStr = c"system.posix_acl_access";
    // Linux xattrs have a 64 KiB VFS value limit. A fixed bounded read also
    // avoids allocation from a size query that could race with another writer.
    const MAX_ACL_BYTES: usize = 65_536;

    #[derive(PartialEq, Eq)]
    pub(super) struct AccessAcl(Option<Vec<u8>>);

    impl AccessAcl {
        pub(super) fn read(file: &File) -> io::Result<Self> {
            let mut bytes = vec![0_u8; MAX_ACL_BYTES];
            // SAFETY: file is live and bytes supplies the specified writable capacity.
            let length = unsafe {
                libc::fgetxattr(
                    file.as_raw_fd(),
                    ACL_NAME.as_ptr(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                )
            };
            if length < 0 {
                let error = io::Error::last_os_error();
                return if error.raw_os_error() == Some(libc::ENODATA) {
                    Ok(Self(None))
                } else {
                    // Unsupported/unreadable ACLs are not evidence of no ACL.
                    Err(error)
                };
            }
            bytes.truncate(length as usize);
            Ok(Self(Some(bytes)))
        }

        pub(super) fn apply(&self, file: &File) -> io::Result<()> {
            let result = if let Some(bytes) = &self.0 {
                // SAFETY: all pointers remain valid throughout this synchronous call.
                unsafe {
                    libc::fsetxattr(
                        file.as_raw_fd(),
                        ACL_NAME.as_ptr(),
                        bytes.as_ptr().cast(),
                        bytes.len(),
                        0,
                    )
                }
            } else {
                // SAFETY: file is live and ACL_NAME is a terminated static string.
                unsafe { libc::fremovexattr(file.as_raw_fd(), ACL_NAME.as_ptr()) }
            };
            if result == 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if self.0.is_none() && error.raw_os_error() == Some(libc::ENODATA) {
                Ok(())
            } else {
                Err(error)
            }
        }

        pub(super) fn verify(&self, file: &File) -> io::Result<()> {
            if Self::read(file)? == *self {
                Ok(())
            } else {
                Err(io::Error::other(
                    "save access ACL did not retain the destination policy",
                ))
            }
        }
    }

    pub(super) fn prepare_private(file: &File) -> io::Result<()> {
        // The creation mode has already masked named grants. Remove those
        // inherited grants before returning the file for any document writes.
        AccessAcl(None).apply(file)
    }
}

#[cfg(target_os = "macos")]
mod access_acl {
    use super::*;
    use std::ffi::{c_int, c_void};
    use std::os::fd::AsRawFd;

    // Darwin's public <sys/acl.h> API is in libSystem but not exposed by libc.
    // Keep opaque ACL allocations owned by this module and release with acl_free.
    const ACL_TYPE_EXTENDED: c_int = 0x100;
    const ACL_FIRST_ENTRY: c_int = 0;
    unsafe extern "C" {
        fn acl_get_fd_np(fd: c_int, acl_type: c_int) -> *mut c_void;
        fn acl_set_fd_np(fd: c_int, acl: *mut c_void, acl_type: c_int) -> c_int;
        fn acl_init(count: c_int) -> *mut c_void;
        fn acl_get_entry(acl: *mut c_void, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
        fn acl_free(acl: *mut c_void) -> c_int;
    }

    pub(super) struct AccessAcl(*mut c_void);

    impl Drop for AccessAcl {
        fn drop(&mut self) {
            // SAFETY: this is a uniquely owned, live ACL allocation from libSystem.
            unsafe {
                acl_free(self.0);
            }
        }
    }

    impl AccessAcl {
        pub(super) fn read(file: &File) -> io::Result<Self> {
            // SAFETY: file remains open; the returned allocation is owned by us.
            let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
            if !acl.is_null() {
                return Ok(Self(acl));
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ENOENT) {
                return Err(error);
            }
            // Darwin reports an absent ACL as ENOENT. Represent it by an empty
            // ACL, which also clears any inherited policy when installed.
            // SAFETY: acl_init returns an owned allocation or null on failure.
            let empty = unsafe { acl_init(0) };
            if empty.is_null() {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self(empty))
            }
        }

        pub(super) fn apply(&self, file: &File) -> io::Result<()> {
            // SAFETY: the descriptor and owned ACL remain live throughout the call.
            if unsafe { acl_set_fd_np(file.as_raw_fd(), self.0, ACL_TYPE_EXTENDED) } == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }

        pub(super) fn verify(&self, _file: &File) -> io::Result<()> {
            // Darwin chmod does not rewrite the extended ACL. The native set
            // above installs the entire ACL and reports any failure.
            Ok(())
        }

        fn has_entries(&self) -> io::Result<bool> {
            let mut entry = std::ptr::null_mut();
            // SAFETY: this ACL is valid and entry points to a writable pointer.
            if unsafe { acl_get_entry(self.0, ACL_FIRST_ENTRY, &mut entry) } == 0 {
                return Ok(true);
            }
            let error = io::Error::last_os_error();
            // Darwin uses EINVAL for the first entry of an empty, valid ACL.
            if error.raw_os_error() == Some(libc::EINVAL) {
                Ok(false)
            } else {
                Err(error)
            }
        }
    }

    pub(super) fn prepare_private(file: &File) -> io::Result<()> {
        // Unlike POSIX ACL masks, Darwin allow ACEs can override mode0600.
        // Never write to a temp that inherited such an ACL, even after clearing
        // it: another principal could already hold an open descriptor.
        if AccessAcl::read(file)?.has_entries()? {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "cannot create a private save file in a directory with an inherited macOS ACL",
            ))
        } else {
            Ok(())
        }
    }
}

#[cfg(target_os = "freebsd")]
mod access_acl {
    use super::*;
    use std::ffi::{c_int, c_void};
    use std::os::fd::AsRawFd;

    // FreeBSD libc selects POSIX.1e or NFSv4 for acl_get_fd. Avoid applying
    // chmod to a nontrivial NFSv4 ACL whose allow/deny semantics it can alter.
    unsafe extern "C" {
        fn acl_get_fd(fd: c_int) -> *mut c_void;
        fn acl_is_trivial_np(acl: *mut c_void, trivial: *mut c_int) -> c_int;
        fn acl_free(acl: *mut c_void) -> c_int;
    }

    pub(super) struct AccessAcl;

    struct NativeAcl(*mut c_void);

    impl Drop for NativeAcl {
        fn drop(&mut self) {
            // SAFETY: this is an owned allocation returned by acl_get_fd.
            unsafe {
                acl_free(self.0);
            }
        }
    }

    impl AccessAcl {
        pub(super) fn read(file: &File) -> io::Result<Self> {
            // SAFETY: file stays open and the returned allocation is owned here.
            let raw = unsafe { acl_get_fd(file.as_raw_fd()) };
            if raw.is_null() {
                let error = io::Error::last_os_error();
                // An unsupported read alone does not prove there is no ACL.
                // Only allow a filesystem that explicitly disables both types.
                // SAFETY: both fpathconf calls use the same live descriptor.
                let acl_disabled = unsafe {
                    libc::fpathconf(file.as_raw_fd(), libc::_PC_ACL_EXTENDED) == 0
                        && libc::fpathconf(file.as_raw_fd(), libc::_PC_ACL_NFS4) == 0
                };
                return if error.raw_os_error() == Some(libc::EOPNOTSUPP) && acl_disabled {
                    Ok(Self)
                } else {
                    Err(error)
                };
            }
            let acl = NativeAcl(raw);
            let mut trivial = 0;
            // SAFETY: acl is a valid owned allocation; trivial is writable.
            if unsafe { acl_is_trivial_np(acl.0, &mut trivial) } != 0 {
                return Err(io::Error::last_os_error());
            }
            if trivial == 1 {
                // The native predicate proves that mode/ownership represent
                // this entire ACL, so the common restoration path is sufficient.
                Ok(Self)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "cannot safely preserve a nontrivial FreeBSD save ACL",
                ))
            }
        }

        pub(super) fn apply(&self, file: &File) -> io::Result<()> {
            // Both the destination and temp must have mode-equivalent policies.
            Self::read(file).map(|_| ())
        }

        pub(super) fn verify(&self, file: &File) -> io::Result<()> {
            Self::read(file).map(|_| ())
        }
    }

    pub(super) fn prepare_private(file: &File) -> io::Result<()> {
        // Reject a nontrivial inherited ACL before returning the empty file for
        // writing; a previously opened descriptor cannot be revoked by chmod.
        AccessAcl::read(file).map(|_| ())
    }
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))
))]
mod access_acl {
    use super::*;

    pub(super) struct AccessAcl;

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "private saves with preserved access ACLs are not supported on this Unix platform",
        )
    }

    impl AccessAcl {
        pub(super) fn read(_file: &File) -> io::Result<Self> {
            Err(unsupported())
        }
        pub(super) fn apply(&self, _file: &File) -> io::Result<()> {
            Err(unsupported())
        }
        pub(super) fn verify(&self, _file: &File) -> io::Result<()> {
            Err(unsupported())
        }
    }

    pub(super) fn prepare_private(_file: &File) -> io::Result<()> {
        Err(unsupported())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    pub(super) struct TestDir(pub(super) PathBuf);

    impl TestDir {
        pub(super) fn new() -> Self {
            Self::under(&std::env::temp_dir())
        }

        pub(super) fn under(parent: &Path) -> Self {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            for _ in 0..MAX_TEMP_ATTEMPTS {
                let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
                let path = parent.join(format!("zpdf-save-test-{}-{id}", std::process::id()));
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
        #[cfg(unix)]
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o660)).unwrap();
        let temp_path;
        {
            let mut save = SaveTemp::new(&destination).unwrap();
            temp_path = save.path.clone();
            save.file_mut().write_all(b"partial output").unwrap();
            #[cfg(unix)]
            assert_eq!(
                fs::metadata(&temp_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(!temp_path.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"original");
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o660
        );
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
    }

    #[test]
    #[cfg(unix)]
    fn commit_preserves_existing_permissions_after_private_writes() {
        for mode in [0o644, 0o660] {
            let dir = TestDir::new();
            let destination = dir.0.join("document.pdf");
            fs::write(&destination, b"original").unwrap();
            fs::set_permissions(&destination, fs::Permissions::from_mode(mode)).unwrap();
            let mut save = SaveTemp::new(&destination).unwrap();
            let temp_path = save.path.clone();
            save.file_mut().write_all(b"updated").unwrap();
            assert_eq!(
                fs::metadata(&temp_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
                mode
            );
            save.file_mut().sync_all().unwrap();
            save.commit().unwrap();
            assert_eq!(fs::read(&destination).unwrap(), b"updated");
            assert!(!temp_path.exists());
            assert_eq!(
                fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
                mode
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn new_destination_remains_private_after_commit() {
        let dir = TestDir::new();
        let destination = dir.0.join("new-document.pdf");
        let mut save = SaveTemp::new(&destination).unwrap();
        let temp_path = save.path.clone();
        save.file_mut().write_all(b"new output").unwrap();
        assert_eq!(
            fs::metadata(&temp_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        save.file_mut().sync_all().unwrap();
        save.commit().unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"new output");
        assert!(!temp_path.exists());
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    #[cfg(unix)]
    fn replacing_a_symlink_does_not_borrow_its_targets_permissions() {
        use std::os::unix::fs::symlink;

        let dir = TestDir::new();
        let destination = dir.0.join("document.pdf");
        let target = dir.0.join("unrelated.pdf");
        fs::write(&target, b"unrelated file").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o664)).unwrap();
        symlink(&target, &destination).unwrap();
        let mut save = SaveTemp::new(&destination).unwrap();
        save.file_mut().write_all(b"updated").unwrap();
        save.file_mut().sync_all().unwrap();
        save.commit().unwrap();
        assert!(fs::symlink_metadata(&destination).unwrap().is_file());
        assert_eq!(fs::read(&destination).unwrap(), b"updated");
        assert_eq!(fs::read(&target).unwrap(), b"unrelated file");
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o664
        );
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

    #[test]
    #[cfg(unix)]
    #[ignore = "requires ZPDF_SAVE_TEST_GID to be a distinct supplementary group"]
    fn commit_preserves_destination_supplementary_group() {
        use std::os::unix::fs::{fchown, MetadataExt};

        let group: u32 = std::env::var("ZPDF_SAVE_TEST_GID")
            .expect("supply the sandbox supplementary group")
            .parse()
            .unwrap();
        let dir = TestDir::new();
        let destination = dir.0.join("document.pdf");
        let original = File::create(&destination).unwrap();
        fchown(&original, None, Some(group)).unwrap();
        original
            .set_permissions(fs::Permissions::from_mode(0o660))
            .unwrap();
        let expected = original.metadata().unwrap();

        let mut save = SaveTemp::new(&destination).unwrap();
        let temporary = save.file_mut().metadata().unwrap();
        assert_ne!(
            temporary.gid(),
            group,
            "fixture must exercise a group change"
        );
        assert_eq!(temporary.mode() & 0o777, 0o600);
        save.file_mut().write_all(b"updated").unwrap();
        save.file_mut().sync_all().unwrap();
        save.commit().unwrap();

        let actual = fs::metadata(&destination).unwrap();
        assert_eq!(actual.uid(), expected.uid());
        assert_eq!(actual.gid(), group);
        assert_eq!(actual.mode() & 0o777, 0o660);
        assert_eq!(fs::read(&destination).unwrap(), b"updated");
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires trusted wrong-owner/wrong-group fixtures and an unprivileged process"]
    fn ownership_preservation_failure_keeps_destination_and_cleans_temp() {
        use std::os::unix::fs::MetadataExt;

        let fixture_dir = PathBuf::from(
            std::env::var_os("ZPDF_SAVE_POLICY_FIXTURE_DIR")
                .expect("supply a trusted writable fixture directory"),
        );
        for name in ["wrong-owner.pdf", "wrong-group.pdf"] {
            let destination = fixture_dir.join(name);
            let before = fs::metadata(&destination).unwrap();
            let mut save = SaveTemp::new(&destination).unwrap();
            let temp_path = save.path.clone();
            save.file_mut()
                .write_all(b"must not replace original")
                .unwrap();
            assert_eq!(save.file_mut().metadata().unwrap().mode() & 0o777, 0o600);
            save.file_mut().sync_all().unwrap();
            assert!(save.commit().is_err(), "{name} must fail before rename");
            let after = fs::metadata(&destination).unwrap();
            assert_eq!(after.ino(), before.ino());
            assert_eq!(
                (after.uid(), after.gid(), after.mode()),
                (before.uid(), before.gid(), before.mode())
            );
            assert_eq!(fs::read(&destination).unwrap(), b"original");
            assert!(!temp_path.exists());
        }
    }

    #[cfg(target_os = "linux")]
    mod linux_acl {
        use super::*;
        use std::os::fd::AsRawFd;

        // Linux's documented POSIX ACL xattr format, independent of the fix.
        fn named_user_acl(user: u32, owner_permissions: u16) -> Vec<u8> {
            let mut bytes = 2_u32.to_le_bytes().to_vec();
            for (tag, permissions, id) in [
                (0x01_u16, owner_permissions, u32::MAX),
                (0x02, 4, user),
                (0x04, 0, u32::MAX),
                (0x10, 4, u32::MAX),
                (0x20, 0, u32::MAX),
            ] {
                bytes.extend_from_slice(&tag.to_le_bytes());
                bytes.extend_from_slice(&permissions.to_le_bytes());
                bytes.extend_from_slice(&id.to_le_bytes());
            }
            bytes
        }

        fn set_acl(file: &File, name: &std::ffi::CStr, bytes: &[u8]) {
            // SAFETY: file is live and both slices remain valid throughout this call.
            let result = unsafe {
                libc::fsetxattr(
                    file.as_raw_fd(),
                    name.as_ptr(),
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    0,
                )
            };
            assert_eq!(
                result,
                0,
                "ACL fixture failed: {}",
                io::Error::last_os_error()
            );
        }

        fn access_acl(file: &File) -> Option<Vec<u8>> {
            let mut bytes = vec![0_u8; 1024];
            // SAFETY: the live fd and writable buffer are valid for the entire call.
            let length = unsafe {
                libc::fgetxattr(
                    file.as_raw_fd(),
                    c"system.posix_acl_access".as_ptr(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                )
            };
            if length == -1 {
                let error = io::Error::last_os_error();
                assert_eq!(error.raw_os_error(), Some(libc::ENODATA));
                return None;
            }
            bytes.truncate(length as usize);
            Some(bytes)
        }

        fn set_parent_default_acl(dir: &TestDir) {
            let directory = File::open(&dir.0).unwrap();
            set_acl(
                &directory,
                c"system.posix_acl_default",
                &named_user_acl(65532, 7),
            );
        }

        #[test]
        fn commit_preserves_destination_acl_instead_of_parent_default_acl() {
            let dir = TestDir::new();
            let destination = dir.0.join("document.pdf");
            fs::write(&destination, b"original").unwrap();
            let original = File::open(&destination).unwrap();
            set_acl(
                &original,
                c"system.posix_acl_access",
                &named_user_acl(65531, 6),
            );
            let expected = access_acl(&original).unwrap();
            set_parent_default_acl(&dir);

            let mut save = SaveTemp::new(&destination).unwrap();
            let temp_path = save.path.clone();
            save.file_mut().write_all(b"updated").unwrap();
            assert_eq!(
                save.file_mut().metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
            save.file_mut().sync_all().unwrap();
            save.commit().unwrap();

            let saved = File::open(&destination).unwrap();
            assert_eq!(access_acl(&saved), Some(expected));
            assert_eq!(
                saved.metadata().unwrap().permissions().mode() & 0o777,
                0o640
            );
            assert_eq!(fs::read(&destination).unwrap(), b"updated");
            assert!(!temp_path.exists());
        }

        #[test]
        fn commit_clears_inherited_acl_when_destination_has_no_acl() {
            let dir = TestDir::new();
            let destination = dir.0.join("document.pdf");
            fs::write(&destination, b"original").unwrap();
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o640)).unwrap();
            assert_eq!(access_acl(&File::open(&destination).unwrap()), None);
            set_parent_default_acl(&dir);

            let mut save = SaveTemp::new(&destination).unwrap();
            save.file_mut().write_all(b"updated").unwrap();
            save.file_mut().sync_all().unwrap();
            save.commit().unwrap();

            let saved = File::open(&destination).unwrap();
            assert_eq!(access_acl(&saved), None);
            assert_eq!(
                saved.metadata().unwrap().permissions().mode() & 0o777,
                0o640
            );
            assert_eq!(fs::read(&destination).unwrap(), b"updated");
        }

        #[test]
        fn new_destination_has_no_inherited_access_acl() {
            let dir = TestDir::new();
            set_parent_default_acl(&dir);
            let destination = dir.0.join("new-document.pdf");
            let mut save = SaveTemp::new(&destination).unwrap();
            assert_eq!(access_acl(save.file_mut()), None);
            save.file_mut().write_all(b"new output").unwrap();
            save.file_mut().sync_all().unwrap();
            save.commit().unwrap();

            let saved = File::open(&destination).unwrap();
            assert_eq!(access_acl(&saved), None);
            assert_eq!(
                saved.metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(fs::read(&destination).unwrap(), b"new output");
        }
    }

    #[cfg(target_os = "macos")]
    mod macos_acl {
        use super::*;
        use std::ffi::{c_int, c_void};
        use std::os::fd::AsRawFd;

        unsafe extern "C" {
            fn acl_init(count: c_int) -> *mut c_void;
            fn acl_free(acl: *mut c_void) -> c_int;
            fn acl_create_entry(acl: *mut *mut c_void, entry: *mut *mut c_void) -> c_int;
            fn acl_set_tag_type(entry: *mut c_void, tag: c_int) -> c_int;
            fn acl_set_qualifier(entry: *mut c_void, qualifier: *const c_void) -> c_int;
            fn acl_get_permset(entry: *mut c_void, set: *mut *mut c_void) -> c_int;
            fn acl_add_perm(set: *mut c_void, permission: c_int) -> c_int;
            fn acl_get_flagset_np(object: *mut c_void, set: *mut *mut c_void) -> c_int;
            fn acl_add_flag_np(set: *mut c_void, flag: c_int) -> c_int;
            fn acl_get_fd_np(fd: c_int, acl_type: c_int) -> *mut c_void;
            fn acl_set_fd_np(fd: c_int, acl: *mut c_void, acl_type: c_int) -> c_int;
            fn acl_size(acl: *mut c_void) -> isize;
            fn acl_copy_ext(buffer: *mut c_void, acl: *mut c_void, size: isize) -> isize;
        }

        struct Acl(*mut c_void);

        impl Drop for Acl {
            fn drop(&mut self) {
                // SAFETY: the ACL came from a successful native allocation.
                if !self.0.is_null() {
                    unsafe {
                        acl_free(self.0);
                    }
                }
            }
        }

        fn set_named_read_acl(file: &File, inherit: bool) {
            // Use an arbitrary stable UUID; the kernel does not require a local
            // account to exist for the ACE to be retained and inherited.
            let qualifier = [0x42_u8; 16];
            // SAFETY: all output pointers and the qualifier are live for each
            // call; the valid ACL owns the returned entry/permission/flag sets.
            unsafe {
                let mut acl = Acl(acl_init(1));
                assert!(!acl.0.is_null());
                let mut entry = std::ptr::null_mut();
                assert_eq!(acl_create_entry(&mut acl.0, &mut entry), 0);
                assert_eq!(acl_set_tag_type(entry, 1), 0); // ACL_EXTENDED_ALLOW
                assert_eq!(acl_set_qualifier(entry, qualifier.as_ptr().cast()), 0);
                let mut permissions = std::ptr::null_mut();
                assert_eq!(acl_get_permset(entry, &mut permissions), 0);
                assert_eq!(acl_add_perm(permissions, 1 << 1), 0); // ACL_READ_DATA
                if inherit {
                    let mut flags = std::ptr::null_mut();
                    assert_eq!(acl_get_flagset_np(entry, &mut flags), 0);
                    assert_eq!(acl_add_flag_np(flags, 1 << 5), 0); // FILE_INHERIT
                }
                assert_eq!(
                    acl_set_fd_np(file.as_raw_fd(), acl.0, 0x100),
                    0,
                    "could not set macOS ACL: {}",
                    io::Error::last_os_error()
                );
            }
        }

        fn acl_bytes(file: &File) -> Vec<u8> {
            // SAFETY: file stays open; acl_size/copy use a live owned ACL and
            // a writable buffer with the exact capacity returned by acl_size.
            unsafe {
                let acl = Acl(acl_get_fd_np(file.as_raw_fd(), 0x100));
                assert!(!acl.0.is_null());
                let length = acl_size(acl.0);
                assert!((1..=65_536).contains(&length));
                let mut bytes = vec![0; length as usize];
                let copied = acl_copy_ext(bytes.as_mut_ptr().cast(), acl.0, length);
                assert!(copied > 0 && copied <= length);
                bytes.truncate(copied as usize);
                bytes
            }
        }

        #[test]
        fn inherited_macos_acl_is_rejected_before_writing_and_cleaned_up() {
            let dir = TestDir::new();
            let destination = dir.0.join("document.pdf");
            fs::write(&destination, b"original").unwrap();
            let directory = File::open(&dir.0).unwrap();
            set_named_read_acl(&directory, true);

            let error = match SaveTemp::new(&destination) {
                Ok(_) => {
                    panic!("a temp with inherited macOS ACEs must not be returned for writing")
                }
                Err(error) => error,
            };
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert!(error.to_string().contains("inherited macOS ACL"));
            assert_eq!(fs::read(&destination).unwrap(), b"original");
            assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
        }

        #[test]
        fn commit_preserves_existing_macos_acl_in_an_ordinary_directory() {
            let dir = TestDir::new();
            let destination = dir.0.join("document.pdf");
            fs::write(&destination, b"original").unwrap();
            fs::set_permissions(&destination, fs::Permissions::from_mode(0o640)).unwrap();
            let original = File::open(&destination).unwrap();
            set_named_read_acl(&original, false);
            let expected_acl = acl_bytes(&original);
            let mut save = SaveTemp::new(&destination).unwrap();
            save.file_mut().write_all(b"updated").unwrap();
            save.file_mut().sync_all().unwrap();
            save.commit().unwrap();

            let saved = File::open(&destination).unwrap();
            assert_eq!(acl_bytes(&saved), expected_acl);
            assert_eq!(
                saved.metadata().unwrap().permissions().mode() & 0o777,
                0o640
            );
            assert_eq!(fs::read(&destination).unwrap(), b"updated");
        }
    }
}
