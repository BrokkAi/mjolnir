//! Reviewer MCP identities shared with launch configuration.
/// The private review MCP server named in the supervisor prompt.
pub const REVIEW_MCP_SERVER_NAME: &str = "mj-review";
/// The dispatch socket inside the worker reviewer directory.
pub const REVIEW_DISPATCH_SOCKET: &str = "review-dispatch.sock";

/// The MCP process inherits its immutable supervisor generation at launch;
/// it cannot accidentally dispatch work into a replacement review.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneDispatchEnvelope {
    pub generation: u64,
    pub dispatch: super::lanes::LaneDispatch,
}
