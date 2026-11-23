use sessionwiki::adapters::Adapter;

use super::native::{NativeCandidate, NativeMetadata, optional_entries};
use super::*;

pub(super) fn is_acp_record(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "json")
        && path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "sessions")
}

pub(super) fn candidates(
    home: &Path,
    adapter: &dyn Adapter,
    cache: &NativeScanCache,
) -> Result<Vec<NativeCandidate>> {
    let mut candidates = Vec::new();
    for entry in optional_entries(&home.join("sessions"))? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file() || !is_acp_record(&path) {
            continue;
        }
        // parent_session_id is serialized after messages. Decode only metadata
        // and skip transcript values without allocating them.
        let metadata = cache.native_metadata(&path, || {
            let file = BufReader::new(fs::File::open(&path)?);
            Ok(Some(
                serde_json::from_reader::<_, NativeMetadata>(file)
                    .with_context(|| format!("read DSH metadata from {}", path.display()))?,
            ))
        })?;
        if let Some(metadata) = metadata
            && let Some(candidate) =
                NativeCandidate::from_metadata(adapter, path.clone(), &path, metadata, None)?
        {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}
