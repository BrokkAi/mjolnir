/// A container runtime that detection found but cannot use, carrying the
/// reason so the Settings notice can name it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RejectedRuntime {
    /// The runtime kind as a runtime stores it (`bare`, `podman`, `docker`,
    /// `apple-container`).
    pub kind: String,
    pub label: String,
    pub detail: String,
    pub remediation: Option<String>,
}
