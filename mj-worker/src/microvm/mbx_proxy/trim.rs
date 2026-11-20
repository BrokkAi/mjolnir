use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use super::{STAGING_PREFIX, sys};

pub(super) fn cache_budget(cache: &Path) -> u64 {
    // mbx 1.30's automatic store budget is 5% of the filesystem rounded
    // down to 5 GiB, with a 5 GiB floor, 500 GiB ceiling, and 20 GiB fallback
    // when the filesystem cannot be measured (mbx's config.rs STORE_BUDGET).
    const GIB: u64 = 1 << 30;
    const INCREMENT: u64 = 5 * GIB;
    match sys::filesystem_bytes(cache) {
        Ok(total) if total != 0 => {
            (total / 100 * 5 / INCREMENT * INCREMENT).clamp(INCREMENT, 500 * GIB)
        }
        Ok(_) => 20 * GIB,
        Err(error) => {
            eprintln!("determine the mbx cache filesystem size: {error:#}; using a 20 GiB budget");
            20 * GIB
        }
    }
}

pub(super) fn trim(cache: &Path, budget: Option<u64>) -> Result<u64> {
    let scan = match fs::read_dir(cache) {
        Ok(scan) => scan,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error).with_context(|| format!("scan {}", cache.display())),
    };
    let budget = budget.unwrap_or_else(|| cache_budget(cache));
    let mut entries = Vec::new();
    let mut total = 0u64;
    let mut freed = 0u64;
    let now = SystemTime::now();
    for entry in scan {
        let entry = entry?;
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            // A proxy may have just published or discarded a staging file.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
        };
        if !metadata.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.as_encoded_bytes();
        if name.starts_with(STAGING_PREFIX.as_bytes()) {
            if now
                .duration_since(metadata.modified()?)
                .is_ok_and(|age| age >= Duration::from_secs(3600))
                && remove(&path)?
            {
                freed += metadata.len();
            }
        } else if name.len() == 64 && name.iter().all(u8::is_ascii_hexdigit) {
            entries.push((metadata.accessed()?, path, metadata.len()));
            total += metadata.len();
        }
    }

    entries.sort_unstable();
    for (_, path, size) in entries {
        if total <= budget {
            break;
        }
        if remove(&path)? {
            freed += size;
        }
        total -= size;
    }
    Ok(freed)
}

fn remove(path: &Path) -> Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("remove mbx cache entry {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, FileTimes};
    use std::io::Read;

    #[test]
    fn mbx_proxy_trim_keeps_live_reader_and_removes_oldest() {
        let cache = tempfile::tempdir().unwrap();
        let older = cache.path().join("a".repeat(64));
        let newer = cache.path().join("b".repeat(64));
        let stale = cache.path().join(format!("{STAGING_PREFIX}abandoned"));
        let active = cache.path().join(format!("{STAGING_PREFIX}active"));
        let directory = cache.path().join("c".repeat(64));
        for path in [&older, &newer, &stale, &active] {
            fs::write(path, [0xa5; 128 * 1024]).unwrap();
        }
        fs::create_dir(&directory).unwrap();
        let mut reader = File::open(&older).unwrap();
        let past = SystemTime::now() - Duration::from_secs(7200);
        reader
            .set_times(FileTimes::new().set_accessed(past))
            .unwrap();
        File::open(&stale)
            .unwrap()
            .set_times(FileTimes::new().set_modified(past))
            .unwrap();
        let bytes = 128 * 1024;
        assert_eq!(trim(cache.path(), Some(bytes)).unwrap(), 2 * bytes);
        assert!(!older.exists());
        assert!(!stale.exists());
        assert!(newer.exists());
        assert!(active.exists());
        assert!(directory.is_dir());
        let mut contents = Vec::new();
        reader.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, vec![0xa5; bytes as usize]);
        assert_eq!(trim(cache.path(), Some(bytes)).unwrap(), 0);
    }
}
