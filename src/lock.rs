use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

pub struct RunLock {
    file: File,
}

impl RunLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create lock directory {}", parent.display()))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("failed to open run lock {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                let mut holder = String::new();
                let _ = file.read_to_string(&mut holder);
                let holder = holder.trim();
                if holder.is_empty() {
                    bail!("another zsnap process holds {}", path.display());
                }
                bail!(
                    "another zsnap process (PID {holder}) holds {}",
                    path.display()
                );
            }
            Err(TryLockError::Error(error)) => {
                return Err(error)
                    .with_context(|| format!("failed to lock run lock {}", path.display()));
            }
        }
        file.set_len(0)
            .with_context(|| format!("failed to truncate lock file {}", path.display()))?;
        file.seek(SeekFrom::Start(0))?;
        writeln!(file, "{}", std::process::id())?;
        file.sync_data()?;
        Ok(Self { file })
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_reports_holder_until_released() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("run/zsnap.lock");

        let first = RunLock::acquire(&path).unwrap();
        let error = RunLock::acquire(&path).err().unwrap().to_string();
        assert_eq!(
            error,
            format!(
                "another zsnap process (PID {}) holds {}",
                std::process::id(),
                path.display()
            )
        );

        drop(first);
        RunLock::acquire(&path).unwrap();
    }
}
