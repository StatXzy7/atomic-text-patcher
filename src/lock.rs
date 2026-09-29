//! Advisory inter-process locks via `flock(2)`.
//!
//! Locks live in a state directory *outside* the project tree, so taking a
//! shared lock for a preview never writes into the user's directory. The lock
//! is released when the handle drops; if the process is killed the kernel
//! releases it automatically.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use crate::error::{io_err, Error, ErrorKind, Result};

#[derive(Debug, Clone, Copy)]
pub enum LockMode {
    /// Multiple readers (previews) may hold a shared lock together.
    Shared,
    /// Only one writer (commit/recover) may hold the lock; it blocks readers.
    Exclusive,
}

pub struct FileLock {
    _file: File,
}

impl FileLock {
    /// Acquire `mode` on `lock_path`.
    ///
    /// `nonblocking=true` maps to `LOCK_NB`: contention becomes
    /// [`ErrorKind::Locked`] instead of waiting.
    pub fn acquire(lock_path: &Path, mode: LockMode, nonblocking: bool) -> Result<FileLock> {
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| io_err(err, format!("创建锁目录 {}", parent.display())))?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(lock_path)
            .map_err(|err| io_err(err, format!("打开锁文件 {}", lock_path.display())))?;

        let mut operation = match mode {
            LockMode::Shared => LOCK_SH,
            LockMode::Exclusive => LOCK_EX,
        };
        if nonblocking {
            operation |= LOCK_NB;
        }

        let rc = unsafe { flock(file.as_raw_fd(), operation) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if nonblocking && err.raw_os_error() == Some(EWOULDBLOCK) {
                return Err(Error::new(
                    ErrorKind::Locked,
                    format!(
                        "目录正被另一个写入会话占用（锁文件 {}）",
                        lock_path.display()
                    ),
                ));
            }
            return Err(io_err(err, format!("获取文件锁 {}", lock_path.display())));
        }
        Ok(FileLock { _file: file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Closing the fd releases the flock; explicit unlock is best-effort.
        let rc = unsafe { flock(self._file.as_raw_fd(), LOCK_UN) };
        let _ = rc;
    }
}

const LOCK_SH: i32 = 1;
const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;
const LOCK_UN: i32 = 8;
// EWOULDBLOCK is 11 on Linux; EAGAIN shares the same number.
const EWOULDBLOCK: i32 = 11;

extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}
