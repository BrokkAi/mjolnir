use super::*;

/// The schema's CHECK constraint admits only known session states.
pub(super) fn stored_session_state(value: &str) -> SessionState {
    SessionState::from_stored(value).expect("schema CHECK admits only known session states")
}

#[cfg(unix)]
pub(super) fn path_to_blob(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}
#[cfg(unix)]
pub(super) fn blob_to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}
#[cfg(windows)]
pub(super) fn path_to_blob(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}
#[cfg(windows)]
pub(super) fn blob_to_path(bytes: &[u8]) -> PathBuf {
    use std::os::windows::ffi::OsStringExt;
    let wide = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect::<Vec<_>>();
    PathBuf::from(std::ffi::OsString::from_wide(&wide))
}

pub(super) trait ValueRefExt<'a> {
    fn blob_or_null(self) -> rusqlite::Result<Option<&'a [u8]>>;
}
impl<'a> ValueRefExt<'a> for rusqlite::types::ValueRef<'a> {
    fn blob_or_null(self) -> rusqlite::Result<Option<&'a [u8]>> {
        match self {
            rusqlite::types::ValueRef::Null => Ok(None),
            value => Ok(Some(value.as_blob()?)),
        }
    }
}
