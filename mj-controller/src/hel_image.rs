//! Compatibility re-exports for image decoding used by controller APIs.
//!
//! The implementation lives in `mj_client` so native and web control surfaces
//! share the same limits, codec behavior, and error handling.

pub use mj_client::image::{OptimizedImage, optimize_image, optimize_rgba};
