pub const MJOLNIR_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn mjolnir_version_label() -> String {
    format!("mjolnir v{MJOLNIR_VERSION}")
}
