//! Native kernel ACL regressions. System utilities are independent fixture/oracle
//! implementations; no ACL mocks or capability-based early returns are used.
use super::{tests::TestDir, *};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::process::Command;

fn command(program: &str, args: &[&str], path: &Path) -> String {
    let output = Command::new(program)
        .args(args)
        .arg(path)
        .env("LC_ALL", "C")
        .output()
        .expect("native ACL fixture tool must be installed");
    assert!(
        output.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn acl(path: &Path) -> Vec<String> {
    #[cfg(target_os = "macos")]
    let output = command("/bin/ls", &["-lde"], path);
    #[cfg(target_os = "freebsd")]
    let output = command("/bin/getfacl", &["-qn"], path);
    output
        .lines()
        .map(str::trim)
        .filter(|line| {
            #[cfg(target_os = "macos")]
            return line
                .split_once(':')
                .is_some_and(|(index, _)| index.parse::<usize>().is_ok());
            #[cfg(target_os = "freebsd")]
            return !line.is_empty() && !line.starts_with('#');
        })
        .map(str::to_owned)
        .collect()
}

fn policy(path: &Path) -> (u32, u32, u32) {
    let metadata = fs::metadata(path).unwrap();
    (metadata.uid(), metadata.gid(), metadata.mode() & 0o7777)
}

fn assert_unprivileged() {
    // SAFETY: geteuid has no arguments or side effects.
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "run ACL tests without root privileges"
    );
}

fn successful_save(destination: &Path, compare_acl: bool) {
    assert_unprivileged();
    let before = policy(destination);
    let before_acl = compare_acl.then(|| acl(destination));
    let before_inode = fs::metadata(destination).unwrap().ino();
    let mut save = SaveTemp::new(destination).unwrap();
    let temporary = save.path.clone();
    assert_eq!(policy(&temporary).2, 0o600);
    save.file_mut().write_all(b"updated").unwrap();
    save.file_mut().sync_all().unwrap();
    save.commit().unwrap();
    assert_eq!(fs::read(destination).unwrap(), b"updated");
    assert_eq!(policy(destination), before);
    assert_ne!(fs::metadata(destination).unwrap().ino(), before_inode);
    if let Some(expected) = before_acl {
        assert_eq!(acl(destination), expected);
    }
    assert!(!temporary.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn macos_preserves_ordered_allow_and_deny_acl() {
    let dir = TestDir::new();
    let destination = dir.0.join("document.pdf");
    fs::write(&destination, b"original").unwrap();
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o640)).unwrap();
    command(
        "/bin/chmod",
        &["+a", "group:everyone allow read,readattr,readsecurity"],
        &destination,
    );
    command(
        "/bin/chmod",
        &["+a", "group:everyone deny write,append"],
        &destination,
    );
    let entries = acl(&destination);
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().any(|e| e.contains(" deny ")));
    assert!(entries.iter().any(|e| e.contains(" allow ")));
    successful_save(&destination, true);
}

#[cfg(target_os = "macos")]
fn macos_rejects_inheritance(entry: &str) {
    assert_unprivileged();
    let dir = TestDir::new();
    let destination = dir.0.join("document.pdf");
    fs::write(&destination, b"original").unwrap();
    let before = policy(&destination);
    command("/bin/chmod", &["+a", entry], &dir.0);
    // Prove the filesystem actually inherits ACEs even with a 0600 creation mode.
    let probe = dir.0.join("probe");
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&probe)
        .unwrap();
    assert!(acl(&probe).iter().any(|e| e.contains("inherited")));
    fs::remove_file(probe).unwrap();
    let error = SaveTemp::new(&destination)
        .err()
        .expect("inherited ACL must reject the empty temp");
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert_eq!(fs::read(&destination).unwrap(), b"original");
    assert_eq!(policy(&destination), before);
    assert!(acl(&destination).is_empty());
    assert_eq!(
        fs::read_dir(&dir.0).unwrap().count(),
        1,
        "failed creation must clean up"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn macos_rejects_inherited_allow_before_writing() {
    macos_rejects_inheritance("group:everyone allow read,file_inherit,only_inherit");
}

#[cfg(target_os = "macos")]
#[test]
fn macos_rejects_inherited_deny_before_writing() {
    macos_rejects_inheritance("group:everyone deny read,file_inherit,only_inherit");
}

#[cfg(target_os = "freebsd")]
fn freebsd_dir(kind: &str) -> TestDir {
    use std::os::fd::AsRawFd;
    assert_unprivileged();
    let root = std::env::var_os("ZPDF_ACL_FIXTURES").expect("CI must mount the three UFS fixtures");
    let dir = TestDir::under(&PathBuf::from(root).join(kind));
    let file = File::open(&dir.0).unwrap();
    // SAFETY: both queries operate on this live directory descriptor.
    let actual = unsafe {
        (
            libc::fpathconf(file.as_raw_fd(), libc::_PC_ACL_EXTENDED),
            libc::fpathconf(file.as_raw_fd(), libc::_PC_ACL_NFS4),
        )
    };
    let expected = match kind {
        "posix" => (1, 0),
        "nfs4" => (0, 1),
        "disabled" => (0, 0),
        _ => panic!("unknown fixture"),
    };
    assert_eq!(
        actual, expected,
        "fixture must use the requested kernel ACL mode"
    );
    dir
}

#[cfg(target_os = "freebsd")]
fn freebsd_success(kind: &str) {
    let dir = freebsd_dir(kind);
    let destination = dir.0.join("document.pdf");
    fs::write(&destination, b"original").unwrap();
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o640)).unwrap();
    successful_save(&destination, kind != "disabled");
}

#[cfg(target_os = "freebsd")]
fn freebsd_rejects_destination(kind: &str, entry: &str) {
    let dir = freebsd_dir(kind);
    let destination = dir.0.join("document.pdf");
    fs::write(&destination, b"original").unwrap();
    command("/bin/setfacl", &["-m", entry], &destination);
    let before_acl = acl(&destination);
    assert!(before_acl.iter().any(|e| e.contains("65532")));
    let before = policy(&destination);
    let inode = fs::metadata(&destination).unwrap().ino();
    let mut save = SaveTemp::new(&destination).unwrap();
    let temporary = save.path.clone();
    save.file_mut()
        .write_all(b"must not replace original")
        .unwrap();
    save.file_mut().sync_all().unwrap();
    let error = save.commit().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert_eq!(fs::read(&destination).unwrap(), b"original");
    assert_eq!(policy(&destination), before);
    assert_eq!(acl(&destination), before_acl);
    assert_eq!(fs::metadata(&destination).unwrap().ino(), inode);
    assert!(!temporary.exists());
}

#[cfg(target_os = "freebsd")]
fn freebsd_rejects_inheritance(kind: &str, args: &[&str]) {
    let dir = freebsd_dir(kind);
    let destination = dir.0.join("document.pdf");
    fs::write(&destination, b"original").unwrap();
    let before_acl = acl(&destination);
    let before = policy(&destination);
    command("/bin/setfacl", args, &dir.0);
    let probe = dir.0.join("probe");
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&probe)
        .unwrap();
    assert!(acl(&probe).iter().any(|e| e.contains("65532")));
    fs::remove_file(probe).unwrap();
    let error = SaveTemp::new(&destination)
        .err()
        .expect("nontrivial inherited ACL must fail before writing");
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert_eq!(fs::read(&destination).unwrap(), b"original");
    assert_eq!(policy(&destination), before);
    assert_eq!(acl(&destination), before_acl);
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
}

#[cfg(target_os = "freebsd")]
macro_rules! freebsd_test {
    ($name:ident, $body:expr) => {
        #[test]
        #[ignore = "requires CI-created UFS mounts; the native ACL job explicitly runs this test"]
        fn $name() {
            $body
        }
    };
}

#[cfg(target_os = "freebsd")]
freebsd_test!(freebsd_accepts_trivial_posix_acl, freebsd_success("posix"));
#[cfg(target_os = "freebsd")]
freebsd_test!(freebsd_accepts_trivial_nfs4_acl, freebsd_success("nfs4"));
#[cfg(target_os = "freebsd")]
freebsd_test!(
    freebsd_accepts_confirmed_disabled_acls,
    freebsd_success("disabled")
);
#[cfg(target_os = "freebsd")]
freebsd_test!(
    freebsd_rejects_nontrivial_posix_acl,
    freebsd_rejects_destination("posix", "user:65532:r--")
);
#[cfg(target_os = "freebsd")]
freebsd_test!(
    freebsd_rejects_nontrivial_nfs4_acl,
    freebsd_rejects_destination("nfs4", "user:65532:read_data::allow")
);
#[cfg(target_os = "freebsd")]
freebsd_test!(
    freebsd_rejects_inherited_posix_acl,
    freebsd_rejects_inheritance(
        "posix",
        &[
            "-d",
            "-m",
            "user::rwx,user:65532:r--,group::---,mask::r--,other::---"
        ]
    )
);
#[cfg(target_os = "freebsd")]
freebsd_test!(
    freebsd_rejects_inherited_nfs4_acl,
    freebsd_rejects_inheritance(
        "nfs4",
        &["-m", "user:65532:read_data:file_inherit/inherit_only:allow"]
    )
);
