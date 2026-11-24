use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, ErrorKind};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use crate::Result;

pub fn user_id() -> u32 {
    // SAFETY: geteuid takes no arguments and has no preconditions.
    unsafe { libc::geteuid() }
}

#[derive(Debug, Clone)]
pub struct Runtime {
    directory: PathBuf,
}

impl Runtime {
    pub fn prepare(directory: Option<PathBuf>) -> Result<Self> {
        let directory =
            directory.unwrap_or_else(|| std::env::temp_dir().join(format!("xaya-{}", user_id())));
        if !directory.is_absolute() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "runtime directory must be absolute",
            )
            .into());
        }
        let name = directory
            .file_name()
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "invalid runtime directory"))?;
        let parent = directory
            .parent()
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "invalid runtime parent"))?
            .canonicalize()?;
        for ancestor in parent.ancestors() {
            let metadata = fs::metadata(ancestor)?;
            if ![0, user_id()].contains(&metadata.uid())
                || (metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0)
            {
                return Err(io::Error::new(
                    ErrorKind::PermissionDenied,
                    "runtime parent is not trusted",
                )
                .into());
            }
        }
        let directory = parent.join(name);
        match DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => (),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error.into()),
        }
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != user_id()
            || metadata.mode() & 0o777 != 0o700
        {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "runtime directory must be owned by this user with mode 0700",
            )
            .into());
        }
        Ok(Self { directory })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub(crate) fn is_default(&self) -> bool {
        std::env::temp_dir()
            .canonicalize()
            .is_ok_and(|parent| self.directory == parent.join(format!("xaya-{}", user_id())))
    }

    pub(crate) fn lock_task_cycle(&self, task_id: &str) -> Result<FileLock> {
        if task_id.is_empty() || !task_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(crate::Error::Protocol("invalid task ID"));
        }
        match FileLock::exclusive(self.private_file(&format!("cycle-{task_id}.lock"), false)?) {
            Err(crate::Error::Io(error)) if error.kind() == ErrorKind::WouldBlock => {
                Err(crate::Error::Protocol("another cycle is already running for this task"))
            },
            result => result,
        }
    }

    pub(crate) fn lock_task_launch(&self, task_id: &str) -> Result<FileLock> {
        if task_id.is_empty() || !task_id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(crate::Error::Protocol("invalid task ID"));
        }
        match FileLock::exclusive(self.private_file(&format!("launch-{task_id}.lock"), false)?) {
            Err(crate::Error::Io(error)) if error.kind() == ErrorKind::WouldBlock => {
                Err(crate::Error::Rpc {
                    code: xaya_agent_proto::CONFLICT,
                    message: "another launch operation is in progress for this task".into(),
                })
            },
            result => result,
        }
    }

    /// Serialize task dispatch across all worktrees of one canonical Git common directory.
    pub(crate) fn lock_project_launch(&self, project: &str) -> Result<FileLock> {
        use sha2::{Digest, Sha256};
        let key = format!("{:x}", Sha256::digest(project.as_bytes()));
        match FileLock::exclusive(self.private_file(&format!("project-launch-{key}.lock"), false)?)
        {
            Err(crate::Error::Io(error)) if error.kind() == ErrorKind::WouldBlock => {
                Err(crate::Error::Rpc {
                    code: xaya_agent_proto::CONFLICT,
                    message: "another task launch is in progress for this project".into(),
                })
            },
            result => result,
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.directory.join("agentd.sock")
    }

    pub(crate) fn private_file(&self, name: &str, append: bool) -> Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .append(append)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.directory.join(name))?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != user_id()
            || metadata.mode() & 0o777 != 0o600
            || metadata.nlink() != 1
        {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "service file must be a private regular file",
            )
            .into());
        }
        Ok(file)
    }

    pub fn connect(&self) -> Result<UnixStream> {
        let metadata = fs::symlink_metadata(self.socket())?;
        if !metadata.file_type().is_socket() || metadata.uid() != user_id() {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "service socket is not private or not owned by this user",
            )
            .into());
        }
        if metadata.mode() & 0o777 != 0o600 {
            // bind creates the path before chmod. Startup clients must wait without trusting it.
            return Err(io::Error::new(
                ErrorKind::WouldBlock,
                "service socket is not ready with mode 0600",
            )
            .into());
        }
        Ok(UnixStream::connect(self.socket())?)
    }

    pub fn bind(&self) -> Result<(UnixListener, Lease)> {
        let lock = FileLock::exclusive(self.private_file("agentd.lock", false)?)?;
        match fs::symlink_metadata(self.socket()) {
            Ok(metadata) => {
                if !metadata.file_type().is_socket() || metadata.uid() != user_id() {
                    return Err(io::Error::new(
                        ErrorKind::AlreadyExists,
                        "refusing to replace a non-service socket path",
                    )
                    .into());
                }
                match UnixStream::connect(self.socket()) {
                    Ok(_) => {
                        return Err(io::Error::new(
                            ErrorKind::AddrInUse,
                            "service socket is active",
                        )
                        .into());
                    },
                    Err(error)
                        if matches!(
                            error.kind(),
                            ErrorKind::ConnectionRefused | ErrorKind::NotFound
                        ) => {},
                    Err(error) => return Err(error.into()),
                }
                fs::remove_file(self.socket())?;
            },
            Err(error) if error.kind() == ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(self.socket())?;
        let metadata = fs::symlink_metadata(self.socket())?;
        let lease = Lease {
            socket: self.socket(),
            device: metadata.dev(),
            inode: metadata.ino(),
            _lock: lock,
        };
        fs::set_permissions(self.socket(), fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok((listener, lease))
    }
}

/// The persistent lock inode serializes both startup and cleanup. Never unlink the lock file.
pub struct Lease {
    socket: PathBuf,
    device: u64,
    inode: u64,
    _lock: FileLock,
}

/// Unlock explicitly: another thread can fork between close-on-exec open and close.
/// A transiently inherited descriptor must not extend ownership until that child execs.
pub(crate) struct FileLock(File);

impl FileLock {
    pub fn exclusive(file: File) -> Result<Self> {
        // SAFETY: file owns a valid descriptor for the entire lock lifetime.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self(file))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: this descriptor still owns the lock; unlock before closing it.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        match fs::symlink_metadata(&self.socket) {
            Ok(metadata)
                if metadata.file_type().is_socket()
                    && metadata.dev() == self.device
                    && metadata.ino() == self.inode =>
            {
                let _ = fs::remove_file(&self.socket);
            },
            _ => (),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn rejects_insecure_directory_and_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("public");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Runtime::prepare(Some(directory.clone())).is_err());
        let link = temp.path().join("link");
        symlink(&directory, &link).unwrap();
        assert!(Runtime::prepare(Some(link)).is_err());
    }

    #[test]
    fn protects_existing_files_and_lock_inode() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        fs::write(runtime.socket(), "keep").unwrap();
        assert!(runtime.bind().is_err());
        assert_eq!(fs::read_to_string(runtime.socket()).unwrap(), "keep");
        fs::remove_file(runtime.socket()).unwrap();
        let (listener, lease) = runtime.bind().unwrap();
        assert!(runtime.bind().is_err());
        let inode = fs::metadata(runtime.directory().join("agentd.lock")).unwrap().ino();
        drop(listener);
        drop(lease);
        let (_listener, _lease) = runtime.bind().unwrap();
        assert_eq!(fs::metadata(runtime.directory().join("agentd.lock")).unwrap().ino(), inode);
    }

    #[test]
    fn forked_child_does_not_extend_a_released_service_lock() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let (listener, lease) = runtime.bind().unwrap();
        let mut pipe = [0; 2];
        // SAFETY: valid output array; the child uses only async-signal-safe libc calls and
        // _exit, without touching Rust allocations or inherited synchronization primitives.
        unsafe {
            assert_eq!(libc::pipe(pipe.as_mut_ptr()), 0);
            let pid = libc::fork();
            assert!(pid >= 0);
            if pid == 0 {
                libc::close(pipe[1]);
                let mut byte = 0u8;
                libc::read(pipe[0], (&mut byte as *mut u8).cast(), 1);
                libc::_exit(0);
            }
            libc::close(pipe[0]);
            drop(listener);
            drop(lease);
            let acquired = runtime.bind();
            libc::close(pipe[1]);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
            assert!(acquired.is_ok(), "child inherited the lock until exec");
        }
    }

    #[test]
    fn cleanup_preserves_replacement_socket_path() {
        let temp = tempfile::tempdir().unwrap();
        let runtime = Runtime::prepare(Some(temp.path().join("run"))).unwrap();
        let (_listener, lease) = runtime.bind().unwrap();
        fs::remove_file(runtime.socket()).unwrap();
        fs::write(runtime.socket(), "replacement").unwrap();
        drop(lease);
        assert_eq!(fs::read_to_string(runtime.socket()).unwrap(), "replacement");
    }
}
