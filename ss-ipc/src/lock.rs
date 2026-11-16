use std::fs::{File, TryLockError};
use std::io;
use std::path::Path;

/// Single-instance guard: an exclusive advisory lock held for the life of the value.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// `Ok(None)` means another process holds it.
    pub fn acquire(path: &Path) -> io::Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Read-write, not append: on Windows `.append(true)` opens the handle with
        // FILE_APPEND_DATA only, and `LockFileEx` needs GENERIC_READ or GENERIC_WRITE,
        // so the lock is refused with ERROR_ACCESS_DENIED.
        let file = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_is_refused_until_the_first_is_dropped() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("ssd.lock");

        let first = InstanceLock::acquire(&path).expect("first").expect("free");
        let second = InstanceLock::acquire(&path).expect("second");
        drop(first);
        let third = InstanceLock::acquire(&path).expect("third");

        assert!(second.is_none());
        assert!(third.is_some());
    }
}
