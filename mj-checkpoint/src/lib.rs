//! Checkpoint capture, verified archives, native relocation and resource packaging.

pub mod archive;
pub mod checkpoint;
pub mod native;
pub mod resources;
pub mod snapshot;
#[cfg(test)]
mod test_support;
