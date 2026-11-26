//! A private folder for one run, created and removed by this crate only.
//!
//! [`WorkDir::create`] makes a new folder with a random name (`babeldoc-run-<hex>`) and mode
//! `0700` under a parent folder, failing rather than reusing one that exists, and keeps open
//! descriptors for the parent, the folder and its `out` subfolder. The engine writes its PDF
//! into `out` and makes its own job folder (which holds a copy of the input) in `engine`, so
//! removing the private folder removes that copy too, however the engine ended. Nothing else
//! of the caller's is ever touched.
//!
//! # The parent must be private
//!
//! The parent is opened once, and the descriptor must show a folder owned by this user that no
//! group or other user can write to by its mode bits, and (on macOS) with no access control
//! list at all. Anything else (`/tmp`, a shared folder, a folder with an ACL entry) is refused
//! with [`CreateError::ParentNotPrivate`]. The app passes a folder it creates itself with mode
//! `0700` in its own data folder.
//!
//! What this assumes, exactly: the parent's effective permissions are what its mode bits and
//! its (empty) ACL say, and every process of this same user is trusted. Only someone who can
//! write to the parent can rename or replace entries in it, so under those assumptions the only
//! processes that could swap the new folder are this user's own, which this crate does not
//! defend against (they can read the user's files anyway). The ACL check is macOS only; on
//! other Unix systems POSIX ACLs are not looked at, so pass a parent that has none.
//!
//! # When the folder may be deleted
//!
//! A new value has no power to delete anything recursively until the folder is known to be the
//! one made here: right after `mkdirat`, the name is looked up without following links, the
//! folder is opened through the parent's descriptor with `O_NOFOLLOW`, and the open descriptor
//! must show a folder owned by this user, empty, with the same device and inode as the name.
//! Only then is it `chmod`ed to `0700` and the recursive removal armed. If any of that fails,
//! the most that is done is `unlinkat(…, AT_REMOVEDIR)` of the name, and only while it still has
//! the identity that was looked up: that removes an empty folder only, never contents.
//!
//! Everything after creation goes through those descriptors, never by resolving the path
//! again: checking that the engine's PDF is a regular file in `out` ([`WorkDir::file_in_out`]),
//! opening it ([`WorkDir::open_in_out`], for a single plain file name only, with `O_NOFOLLOW`
//! and `O_NONBLOCK`, so a link or a FIFO in its place is refused instead of followed or waited
//! on), and removing the folder ([`WorkDir::remove`]). Removal deletes the contents through the
//! folder's own descriptor (it is the verified folder even if it was renamed meanwhile), then
//! the folder itself from the parent's descriptor, only while the name there still is the same
//! folder (device and inode). So a folder swapped in under the same name, or a symbolic link,
//! is never followed or deleted. Dropping a [`WorkDir`] removes it.
//!
//! What remains: another process of this same user can still rename or replace things inside
//! the private folder while the engine runs (the engine itself is such a process). Those
//! changes are confined to the folder, and the PDF is accepted only after the checks in
//! `run::is_pdf_made_here`.

use std::ffi::{CStr, CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// The engine's output folder inside the private folder.
const OUT: &str = "out";
/// The folder the engine makes its job folder in (`BABELDOC_ENGINE_JOB_PARENT`).
const ENGINE: &str = "engine";
/// How deep [`WorkDir::remove`] goes (BabelDOC's working files in the engine's job folder are
/// a few levels deep).
const MAX_DEPTH: usize = 32;

/// Why [`WorkDir::create`] failed.
#[derive(Debug)]
pub(crate) enum CreateError {
    /// The parent is not a folder owned by this user that only this user can write to.
    ParentNotPrivate,
    /// Anything else (the parent is missing, the folder could not be made or verified).
    Io(io::Error),
}

impl From<io::Error> for CreateError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// See the module documentation.
pub struct WorkDir {
    path: PathBuf,
    name: CString,
    parent: OwnedFd,
    root: OwnedFd,
    root_id: (u64, u64),
    /// `None` only while [`WorkDir::create`] has not finished (then the value is dropped).
    out: Option<OwnedFd>,
    out_id: (u64, u64),
    removed: bool,
}

impl std::fmt::Debug for WorkDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkDir").field("path", &self.path).finish()
    }
}

impl WorkDir {
    /// Creates a new private folder in `parent` (which must exist and be private, see the
    /// module documentation).
    pub(crate) fn create(parent: &Path) -> Result<Self, CreateError> {
        let parent_path = std::path::absolute(parent)?;
        let parent_fd = open_dir(None, &cstring(parent_path.as_os_str())?, true)?;
        if !is_private_parent(parent_fd.as_raw_fd())? {
            return Err(CreateError::ParentNotPrivate);
        }
        let name = format!("babeldoc-run-{}", crate::endpoint::random_hex(8)?);
        let name_c = CString::new(name.clone()).expect("no NUL");
        // SAFETY: a valid descriptor and a NUL-terminated name.
        check(unsafe { libc::mkdirat(parent_fd.as_raw_fd(), name_c.as_ptr(), 0o700) })?;
        let (root, root_id) = match open_new_dir(parent_fd.as_raw_fd(), &name_c) {
            Ok(root) => root,
            Err((error, seen)) => {
                remove_empty_if(parent_fd.as_raw_fd(), &name_c, seen);
                return Err(error.into());
            }
        };
        // Verified, with its identity from the check itself: from here on the value owns the
        // folder and dropping it removes it, before anything else can fail.
        let mut dir = Self {
            path: parent_path.join(&name),
            name: name_c,
            parent: parent_fd,
            root,
            root_id,
            out: None,
            out_id: (0, 0),
            removed: false,
        };
        let (out, out_id) = dir.make_sub(OUT)?;
        dir.out = Some(out);
        dir.out_id = out_id;
        dir.make_sub(ENGINE)?;
        Ok(dir)
    }

    /// Makes the new private subfolder `name` and proves it is the one made.
    fn make_sub(&self, name: &str) -> io::Result<(OwnedFd, (u64, u64))> {
        #[cfg(test)]
        if tests::FAIL_SUBFOLDER.with(|fail| fail.get()) {
            return Err(io::Error::from_raw_os_error(libc::EMFILE));
        }
        let name = CString::new(name).expect("no NUL");
        // SAFETY: a valid descriptor and a NUL-terminated name.
        check(unsafe { libc::mkdirat(self.root.as_raw_fd(), name.as_ptr(), 0o700) })?;
        open_new_dir(self.root.as_raw_fd(), &name).map_err(|(e, _)| e)
    }

    /// The descriptor of `out` (`-1`, which every call refuses, never happens after `create`).
    fn out_fd(&self) -> RawFd {
        self.out.as_ref().map_or(-1, |fd| fd.as_raw_fd())
    }

    /// The private folder.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The folder the engine writes into (`--output`).
    pub fn out_path(&self) -> PathBuf {
        self.path.join(OUT)
    }

    /// The folder the engine makes its job folder in (`BABELDOC_ENGINE_JOB_PARENT`): private,
    /// next to `out`, removed with the rest.
    pub fn engine_path(&self) -> PathBuf {
        self.path.join(ENGINE)
    }

    /// Whether both names still lead to the folders created here (nothing was swapped).
    pub(crate) fn intact(&self) -> bool {
        let at = |fd: RawFd, name: &CStr| lstat_at(fd, name).map(|s| id(&s)).ok();
        let out = CString::new(OUT).expect("no NUL");
        at(self.parent.as_raw_fd(), &self.name) == Some(self.root_id)
            && at(self.root.as_raw_fd(), &out) == Some(self.out_id)
            && self.out.is_some()
    }

    /// The file `name` in `out` when it is a single plain name of a regular file owned by this
    /// user (not a link).
    pub(crate) fn file_in_out(&self, name: &OsStr) -> bool {
        let Ok(name) = plain_name(name) else {
            return false;
        };
        lstat_at(self.out_fd(), &name).is_ok_and(|s| is_own_file(&s))
    }

    /// Opens the regular file `name` in `out` for reading. `name` must be a single plain file
    /// name (no `/`, not `.` or `..`); the file is opened without following a link and without
    /// blocking (a FIFO in its place is refused, not waited on), and must be a regular file
    /// owned by this user.
    pub(crate) fn open_in_out(&self, name: &OsStr) -> io::Result<File> {
        let name = plain_name(name)?;
        // SAFETY: a valid descriptor and a NUL-terminated name.
        let fd = unsafe {
            libc::openat(
                self.out_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        check(fd)?;
        // SAFETY: `openat` returned a new descriptor that nothing else owns.
        let file = unsafe { File::from_raw_fd(fd) };
        if !is_own_file(&fstat(file.as_raw_fd())?) {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        // Reads of a regular file never block; clear the flag so the caller gets a plain file.
        // SAFETY: a valid descriptor this function owns.
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
        check(flags)?;
        // SAFETY: as above.
        check(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) })?;
        Ok(file)
    }

    /// Removes the folder and everything in it (see the module documentation). An error means
    /// something is left at [`WorkDir::path`].
    pub fn remove(mut self) -> io::Result<()> {
        self.remove_now()
    }

    fn remove_now(&mut self) -> io::Result<()> {
        if self.removed {
            return Ok(());
        }
        // The contents of the verified folder, through its own descriptor: this is the folder
        // made here even if its name was moved.
        clear(self.root.as_raw_fd(), 0)?;
        let stat = lstat_at(self.parent.as_raw_fd(), &self.name)?;
        if id(&stat) != self.root_id {
            // Not ours any more: leave it alone.
            return Err(io::Error::from(io::ErrorKind::Other));
        }
        // SAFETY: a valid descriptor and a NUL-terminated name.
        check(unsafe {
            libc::unlinkat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        })?;
        self.removed = true;
        Ok(())
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = self.remove_now();
    }
}

/// A folder owned by this user that neither its group nor others can write to by its mode, and
/// (macOS) with no ACL entries. See the module documentation for what this assumes.
fn is_private_parent(fd: RawFd) -> io::Result<bool> {
    let stat = fstat(fd)?;
    let by_mode = (stat.st_mode & libc::S_IFMT) == libc::S_IFDIR
        // SAFETY: no arguments.
        && stat.st_uid == unsafe { libc::geteuid() }
        && stat.st_mode & 0o022 == 0;
    Ok(by_mode && !has_acl(fd)?)
}

/// Whether the file open at `fd` has any extended ACL entry (an entry can grant others the
/// right to add or delete entries whatever the mode says).
#[cfg(target_os = "macos")]
fn has_acl(fd: RawFd) -> io::Result<bool> {
    use std::ffi::{c_int, c_void};
    // `<sys/acl.h>`, in libSystem.
    unsafe extern "C" {
        fn acl_get_fd_np(fd: c_int, kind: c_int) -> *mut c_void;
        fn acl_get_entry(acl: *mut c_void, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
        fn acl_free(object: *mut c_void) -> c_int;
    }
    const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;
    const ACL_FIRST_ENTRY: c_int = 0;
    // SAFETY: a valid descriptor.
    let acl = unsafe { acl_get_fd_np(fd, ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        // No ACL at all.
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(false)
        } else {
            Err(error)
        };
    }
    let mut entry = std::ptr::null_mut();
    // SAFETY: a valid ACL and room for the entry.
    let found = unsafe { acl_get_entry(acl, ACL_FIRST_ENTRY, &mut entry) } == 0;
    // SAFETY: the ACL `acl_get_fd_np` returned, freed once.
    unsafe { acl_free(acl) };
    Ok(found)
}

/// Not looked at outside macOS (see the module documentation).
#[cfg(not(target_os = "macos"))]
fn has_acl(_fd: RawFd) -> io::Result<bool> {
    Ok(false)
}

fn is_own_file(stat: &libc::stat) -> bool {
    // SAFETY: no arguments.
    (stat.st_mode & libc::S_IFMT) == libc::S_IFREG && stat.st_uid == unsafe { libc::geteuid() }
}

/// Opens the folder `name` just made in `at` and proves it is that folder: the name (not
/// followed) and the open descriptor show the same device and inode, a folder owned by this
/// user, with nothing in it. Then sets its mode to `0700`. Returns the descriptor with the
/// verified identity; on failure, also the identity the name had when it was looked up, if it
/// was.
#[allow(clippy::type_complexity)]
fn open_new_dir(
    at: RawFd,
    name: &CStr,
) -> Result<(OwnedFd, (u64, u64)), (io::Error, Option<(u64, u64)>)> {
    let seen = lstat_at(at, name).map_err(|e| (e, None))?;
    let seen_id = id(&seen);
    let fail = |e: io::Error| (e, Some(seen_id));
    let denied = || fail(io::Error::from(io::ErrorKind::PermissionDenied));
    // SAFETY: no arguments.
    let uid = unsafe { libc::geteuid() };
    if (seen.st_mode & libc::S_IFMT) != libc::S_IFDIR || seen.st_uid != uid {
        return Err(denied());
    }
    let fd = open_dir(Some(at), name, false).map_err(fail)?;
    let stat = fstat(fd.as_raw_fd()).map_err(fail)?;
    if id(&stat) != seen_id
        || (stat.st_mode & libc::S_IFMT) != libc::S_IFDIR
        || stat.st_uid != uid
        || !names(fd.as_raw_fd()).map_err(fail)?.is_empty()
    {
        return Err(denied());
    }
    // SAFETY: a descriptor this function owns.
    check(unsafe { libc::fchmod(fd.as_raw_fd(), 0o700) }).map_err(fail)?;
    if fstat(fd.as_raw_fd()).map_err(fail)?.st_mode & 0o777 != 0o700 {
        return Err(denied());
    }
    Ok((fd, seen_id))
}

/// Removes the empty folder `name` in `at` while it still has the identity `seen` (nothing is
/// done when it was never looked up). `AT_REMOVEDIR` never removes contents.
fn remove_empty_if(at: RawFd, name: &CStr, seen: Option<(u64, u64)>) {
    let Some(seen) = seen else {
        return;
    };
    if lstat_at(at, name).is_ok_and(|s| id(&s) == seen) {
        // SAFETY: a valid descriptor and a NUL-terminated name.
        unsafe { libc::unlinkat(at, name.as_ptr(), libc::AT_REMOVEDIR) };
    }
}

/// `name` as a C string when it is one plain file name: not empty, no `/`, not `.` or `..`.
fn plain_name(name: &OsStr) -> io::Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.contains(&b'/') || bytes == b"." || bytes == b".." {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    cstring(name)
}

/// Removes everything in the folder open at `dir`, without following links.
fn clear(dir: RawFd, depth: usize) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(io::Error::from(io::ErrorKind::Other));
    }
    let mut result = Ok(());
    for name in names(dir)? {
        let stat = match lstat_at(dir, &name) {
            Ok(stat) => stat,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                result = Err(e);
                continue;
            }
        };
        let removed = if (stat.st_mode & libc::S_IFMT) == libc::S_IFDIR {
            open_dir(Some(dir), &name, false)
                .and_then(|sub| clear(sub.as_raw_fd(), depth + 1))
                // SAFETY: a valid descriptor and a NUL-terminated name.
                .and_then(|()| {
                    check(unsafe { libc::unlinkat(dir, name.as_ptr(), libc::AT_REMOVEDIR) })
                })
        } else {
            // SAFETY: as above.
            check(unsafe { libc::unlinkat(dir, name.as_ptr(), 0) })
        };
        if let Err(e) = removed {
            result = Err(e);
        }
    }
    result
}

/// The names in the folder open at `dir` (without `.` and `..`).
fn names(dir: RawFd) -> io::Result<Vec<CString>> {
    // `fdopendir` takes the descriptor over; give it a duplicate.
    // SAFETY: a valid descriptor.
    let copy = unsafe { libc::fcntl(dir, libc::F_DUPFD_CLOEXEC, 0) };
    check(copy)?;
    // SAFETY: `copy` is a new descriptor for a folder; on success the stream owns it.
    let stream = unsafe { libc::fdopendir(copy) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: `fdopendir` failed, so `copy` is still ours.
        unsafe { libc::close(copy) };
        return Err(error);
    }
    // The stream starts where the descriptor is; read from the beginning.
    // SAFETY: a valid stream.
    unsafe { libc::rewinddir(stream) };
    let mut names = Vec::new();
    let result = loop {
        // `readdir` returns NULL both at the end and on an error; only `errno` tells them
        // apart, so clear it first.
        set_errno(0);
        // SAFETY: a valid stream; the entry is read before the next call.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            break match error.raw_os_error() {
                Some(0) | None => Ok(()),
                Some(_) => Err(error),
            };
        }
        // SAFETY: `d_name` is NUL-terminated.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(name.to_owned());
        }
    };
    // SAFETY: closes the stream and `copy` with it.
    unsafe { libc::closedir(stream) };
    result.map(|()| names)
}

fn set_errno(value: libc::c_int) {
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly"
    ))]
    // SAFETY: the thread's own errno.
    unsafe {
        *libc::__error() = value
    };
    #[cfg(any(target_os = "linux", target_os = "emscripten"))]
    // SAFETY: as above.
    unsafe {
        *libc::__errno_location() = value
    };
    #[cfg(target_os = "android")]
    // SAFETY: as above.
    unsafe {
        *libc::__errno() = value
    };
}

fn open_dir(at: Option<RawFd>, name: &CStr, follow: bool) -> io::Result<OwnedFd> {
    let mut flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    if !follow {
        flags |= libc::O_NOFOLLOW;
    }
    // SAFETY: a valid descriptor (or the current folder) and a NUL-terminated name.
    let fd = unsafe { libc::openat(at.unwrap_or(libc::AT_FDCWD), name.as_ptr(), flags) };
    check(fd)?;
    // SAFETY: `openat` returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn fstat(fd: RawFd) -> io::Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: a valid descriptor and room for the result.
    check(unsafe { libc::fstat(fd, stat.as_mut_ptr()) })?;
    // SAFETY: `fstat` succeeded and filled it.
    Ok(unsafe { stat.assume_init() })
}

fn lstat_at(dir: RawFd, name: &CStr) -> io::Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: a valid descriptor, a NUL-terminated name and room for the result.
    check(unsafe {
        libc::fstatat(
            dir,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })?;
    // SAFETY: `fstatat` succeeded and filled it.
    Ok(unsafe { stat.assume_init() })
}

fn id(stat: &libc::stat) -> (u64, u64) {
    #[allow(clippy::unnecessary_cast)]
    (stat.st_dev as u64, stat.st_ino as u64)
}

fn cstring(text: &OsStr) -> io::Result<CString> {
    CString::new(text.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

fn check(result: libc::c_int) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::os::unix::fs::{PermissionsExt, symlink};

    thread_local! {
        /// Makes the subfolders fail, as running out of descriptors would.
        pub(super) static FAIL_SUBFOLDER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// Once the folder is verified, a failure while making its subfolders (such
    /// as running out of descriptors) still removes it.
    #[test]
    fn a_failure_after_the_folder_is_made_removes_it() {
        let parent = TempDir::new();
        FAIL_SUBFOLDER.with(|fail| fail.set(true));
        let result = WorkDir::create(parent.path());
        FAIL_SUBFOLDER.with(|fail| fail.set(false));
        assert!(matches!(result, Err(CreateError::Io(_))));
        assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
    }

    /// The engine's job folder goes in a private `engine` folder next to `out`,
    /// and goes with the rest.
    #[test]
    fn the_engine_gets_a_private_folder_that_is_removed_with_the_rest() {
        let parent = TempDir::new();
        let dir = WorkDir::create(parent.path()).unwrap();
        let engine = dir.engine_path();
        assert_eq!(engine.parent(), Some(dir.path()));
        let mode = std::fs::metadata(&engine).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        // Deeper than the engine's own `out`: BabelDOC's working files.
        let deep = engine.join("babeldoc-engine-x/work/input/a/b/c/d/e/f/g");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(engine.join("babeldoc-engine-x/input.pdf"), b"%PDF").unwrap();
        std::fs::write(deep.join("page.json"), b"{}").unwrap();
        dir.remove().unwrap();
        assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
    }

    /// An ACL entry on the parent can let others add or delete entries whatever
    /// the mode says, so a parent with one is refused.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_parent_with_an_acl_is_refused() {
        let parent = TempDir::new();
        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(WorkDir::create(parent.path()).is_ok());
        let chmod = |args: &[&str]| {
            let status = std::process::Command::new("/bin/chmod")
                .args(args)
                .arg(parent.path())
                .status()
                .unwrap();
            assert!(status.success());
        };
        chmod(&[
            "+a",
            "everyone allow add_subdirectory,add_file,delete_child",
        ]);
        assert!(matches!(
            WorkDir::create(parent.path()),
            Err(CreateError::ParentNotPrivate)
        ));
        // Even a deny entry: the rule is "no ACL", not a reading of the entries.
        chmod(&["-N"]);
        chmod(&["+a", "everyone deny delete"]);
        assert!(matches!(
            WorkDir::create(parent.path()),
            Err(CreateError::ParentNotPrivate)
        ));
        chmod(&["-N"]);
        let names: Vec<_> = std::fs::read_dir(parent.path()).unwrap().collect();
        // Only the folder from the first, accepted, create was made (and removed on drop).
        assert!(names.is_empty(), "{names:?}");
    }

    #[test]
    fn a_new_folder_is_private_and_removed_with_its_contents() {
        let parent = TempDir::new();
        let dir = WorkDir::create(parent.path()).unwrap();
        let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        assert!(dir.out_path().is_dir());
        std::fs::write(dir.out_path().join("a.pdf"), b"x").unwrap();
        std::fs::create_dir_all(dir.path().join("deep/er")).unwrap();
        std::fs::write(dir.path().join("deep/er/f"), b"x").unwrap();
        assert!(dir.intact());
        assert!(dir.file_in_out(OsStr::new("a.pdf")));
        let path = dir.path().to_owned();
        dir.remove().unwrap();
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
    }

    /// A folder that is not the one just made (it has contents, or is a link) is
    /// never taken over, and the failed start removes nothing in it.
    #[test]
    fn a_folder_that_is_not_new_and_empty_is_not_taken_over() {
        let parent = TempDir::new();
        let parent_fd = open_dir(None, &cstring(parent.path().as_os_str()).unwrap(), true).unwrap();
        let theirs = parent.path().join("theirs");
        std::fs::create_dir(&theirs).unwrap();
        std::fs::write(theirs.join("keep"), b"keep").unwrap();
        let name = CString::new("theirs").unwrap();
        let (_, seen) = open_new_dir(parent_fd.as_raw_fd(), &name).unwrap_err();
        // At most an empty folder is removed: this one has contents and stays as it was.
        remove_empty_if(parent_fd.as_raw_fd(), &name, seen);
        assert_eq!(std::fs::read(theirs.join("keep")).unwrap(), b"keep");
        let mode = std::fs::metadata(&theirs).unwrap().permissions().mode();
        assert_ne!(mode & 0o777, 0o700, "not chmodded before the check");

        let link = CString::new("link").unwrap();
        symlink(&theirs, parent.path().join("link")).unwrap();
        assert!(open_new_dir(parent_fd.as_raw_fd(), &link).is_err());
        assert_eq!(std::fs::read(theirs.join("keep")).unwrap(), b"keep");
    }

    #[test]
    fn a_parent_others_can_write_to_is_refused() {
        let parent = TempDir::new();
        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o757)).unwrap();
        assert!(matches!(
            WorkDir::create(parent.path()),
            Err(CreateError::ParentNotPrivate)
        ));
        let file = parent.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(WorkDir::create(&file).is_err());
        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(WorkDir::create(parent.path()).is_ok());
    }

    /// Only a single plain name in `out` is opened; paths that leave it, links
    /// and FIFOs are refused without blocking.
    #[test]
    fn only_a_plain_regular_file_in_out_is_opened() {
        let parent = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.path().join("victim.pdf"), b"keep").unwrap();
        let dir = WorkDir::create(parent.path()).unwrap();
        std::fs::write(dir.out_path().join("a.pdf"), b"x").unwrap();
        std::fs::write(dir.path().join("up.pdf"), b"x").unwrap();
        assert!(dir.open_in_out(OsStr::new("a.pdf")).is_ok());
        let victim = outside.path().join("victim.pdf");
        for name in [
            OsStr::new("../up.pdf"),
            OsStr::new(""),
            OsStr::new("."),
            OsStr::new(".."),
            victim.as_os_str(),
            OsStr::new("sub/a.pdf"),
        ] {
            assert!(dir.open_in_out(name).is_err(), "{name:?}");
            assert!(!dir.file_in_out(name), "{name:?}");
        }
        let fifo = CString::new(dir.out_path().join("f.pdf").as_os_str().as_bytes()).unwrap();
        // SAFETY: a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert!(dir.open_in_out(OsStr::new("f.pdf")).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(!dir.file_in_out(OsStr::new("f.pdf")));
    }

    #[test]
    fn two_runs_never_share_a_folder() {
        let parent = TempDir::new();
        let a = WorkDir::create(parent.path()).unwrap();
        let b = WorkDir::create(parent.path()).unwrap();
        assert_ne!(a.path(), b.path());
    }

    #[test]
    fn links_are_not_followed_and_a_swapped_folder_is_left_alone() {
        let parent = TempDir::new();
        let outside = TempDir::new();
        std::fs::write(outside.path().join("victim.pdf"), b"keep").unwrap();
        let dir = WorkDir::create(parent.path()).unwrap();
        // A link to a file and to a folder outside: neither is a file in `out`, and removing
        // the private folder removes the links, not what they point at.
        symlink(
            outside.path().join("victim.pdf"),
            dir.out_path().join("link.pdf"),
        )
        .unwrap();
        symlink(outside.path(), dir.path().join("folder-link")).unwrap();
        assert!(!dir.file_in_out(OsStr::new("link.pdf")));
        assert!(dir.open_in_out(OsStr::new("link.pdf")).is_err());
        drop(dir);
        assert_eq!(
            std::fs::read(outside.path().join("victim.pdf")).unwrap(),
            b"keep"
        );
        assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);

        // The private folder renamed away and another put in its place under the same name.
        let dir = WorkDir::create(parent.path()).unwrap();
        let path = dir.path().to_owned();
        std::fs::rename(&path, parent.path().join("moved")).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("theirs"), b"keep").unwrap();
        assert!(!dir.intact());
        assert!(dir.remove().is_err());
        assert_eq!(std::fs::read(path.join("theirs")).unwrap(), b"keep");
        // The moved original was emptied through its descriptor.
        assert_eq!(
            std::fs::read_dir(parent.path().join("moved"))
                .unwrap()
                .count(),
            0
        );
    }
}
