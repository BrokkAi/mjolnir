use super::*;

/// Reads only take the cache lock; disk writes run on a blocking worker.
#[derive(Default)]
pub(in crate::server) struct ViewerRevocations {
    entries: Mutex<BTreeMap<String, u64>>,
    writer: Mutex<()>,
    path: Option<PathBuf>,
}

impl ViewerRevocations {
    pub(in crate::server) fn load(path: PathBuf) -> AnyResult<Self> {
        let mut entries: BTreeMap<String, u64> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("decode viewer revocations")?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error).context("read viewer revocations"),
        };
        let now = now_unix();
        entries.retain(|_, expiry| *expiry > now);
        Ok(Self {
            entries: Mutex::new(entries),
            writer: Mutex::new(()),
            path: Some(path),
        })
    }

    pub(in crate::server) fn contains(&self, viewer: &str, now: u64) -> bool {
        self.entries
            .lock()
            .expect("viewer revocations poisoned")
            .get(viewer)
            .is_some_and(|expiry| *expiry > now)
    }

    /// Call from blocking work. Failed persistence still revokes in this process.
    pub(in crate::server) fn revoke(
        &self,
        viewer: String,
        minimum_expiry: u64,
        renewal_ttl: Duration,
    ) -> AnyResult<()> {
        let _writer = self
            .writer
            .lock()
            .expect("viewer revocation writer poisoned");
        let snapshot = {
            let now = now_unix();
            let mut entries = self.entries.lock().expect("viewer revocations poisoned");
            entries.retain(|_, expiry| *expiry > now);
            let until = entries.entry(viewer).or_default();
            *until = (*until)
                .max(minimum_expiry)
                .max(now.saturating_add(renewal_ttl.as_secs()));
            entries.clone()
        };
        if let Some(path) = &self.path {
            mj_core::config::atomic_write(path, &serde_json::to_vec(&snapshot)?)
                .context("persist viewer revocation")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revocations_prune_expired_entries_and_preserve_longer_expiries() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("revocations.json");
        let now = now_unix();
        std::fs::write(
            &path,
            serde_json::to_vec(&BTreeMap::from([
                ("expired", now - 1),
                ("active", now + 3600),
            ]))
            .unwrap(),
        )
        .unwrap();
        let store = ViewerRevocations::load(path.clone()).unwrap();
        assert!(!store.contains("expired", now));
        store
            .revoke("active".into(), now + 60, Duration::ZERO)
            .unwrap();
        store
            .revoke("new".into(), now + 60, Duration::from_secs(7200))
            .unwrap();
        let saved: BTreeMap<String, u64> =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(!saved.contains_key("expired"));
        assert_eq!(saved["active"], now + 3600);
        assert!(saved["new"] >= now + 7200);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn concurrent_logouts_are_all_retained_after_reload() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("revocations.json");
        let store = Arc::new(ViewerRevocations::load(path.clone()).unwrap());
        let expiry = now_unix() + 3600;
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let store = store.clone();
                std::thread::spawn(move || store.revoke(i.to_string(), expiry, Duration::ZERO))
            })
            .collect();
        for thread in threads {
            thread.join().unwrap().unwrap();
        }
        let reloaded = ViewerRevocations::load(path).unwrap();
        for i in 0..8 {
            assert!(reloaded.contains(&i.to_string(), now_unix()));
        }
    }

    #[test]
    fn corrupt_or_unreadable_revocations_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("revocations.json");
        std::fs::write(&path, b"not json").unwrap();
        assert!(ViewerRevocations::load(path).is_err());
        assert!(ViewerRevocations::load(directory.path().to_path_buf()).is_err());
    }
}
