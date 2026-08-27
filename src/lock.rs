use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use fs2::FileExt;

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
        if let Err(error) = file.try_lock_exclusive() {
            let mut holder = String::new();
            let _ = file.read_to_string(&mut holder);
            let holder = holder.trim();
            if holder.is_empty() {
                bail!("another zsnap process holds {}: {error}", path.display());
            }
            bail!(
                "another zsnap process (PID {holder}) holds {}: {error}",
                path.display()
            );
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
        let _ = FileExt::unlock(&self.file);
    }
}
