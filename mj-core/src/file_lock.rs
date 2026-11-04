//! File locks whose release belongs to their Rust owner, even across fork.

use std::fs::File;
use std::io;

/// Owns an already locked file. Explicit unlock releases the open file
/// description's lock even if an unrelated fork still holds a descriptor.
#[derive(Debug)]
pub struct FileLock {
    file: Option<File>,
}

impl FileLock {
    /// Transfer ownership immediately after successfully locking `file`.
    pub fn new(file: File) -> Self {
        Self { file: Some(file) }
    }

    /// Inspect the locked file without duplicating ownership of its lock.
    pub fn file(&self) -> &File {
        self.file.as_ref().expect("file lock has not been released")
    }

    /// Release now and report an unlock failure to the caller.
    pub fn release(mut self) -> io::Result<()> {
        self.file
            .take()
            .expect("file lock has not been released")
            .unlock()
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take()
            && let Err(error) = file.unlock()
        {
            tracing::warn!(%error, "could not release file lock");
        }
    }
}
