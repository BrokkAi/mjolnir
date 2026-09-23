//! `mj acp`: an ACP agent whose turns run as Mjolnir sessions.
//!
//! A program that speaks the Agent Client Protocol normally starts a coding
//! harness as a child process and talks to it over standard input and output.
//! This module makes Mjolnir look like one of those harnesses: the consumer
//! starts `mj acp` instead, and every session it creates is a Mjolnir session,
//! which means it runs on a configured target, appears in `mj sessions`, is
//! indexed for search, and can be watched, steered, and resumed like any other.
//!
//! Everything here is a client of the documented HTTP API rather than of the
//! daemon's internals. That is deliberate for two reasons: the adapter cannot
//! depend on behavior no other consumer can, and it stays an honest test of the
//! contract every other consumer sees.
//!
//! Standard output carries the protocol and nothing else. Mjolnir's diagnostic
//! logging goes to a file, and its failures to standard error.

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{AgentCapabilities, InitializeRequest, InitializeResponse};
use agent_client_protocol::{Agent, Stdio};
use anyhow::{Context, Result};

/// Serve the Agent Client Protocol on standard input and output.
///
/// Returns when the consumer closes its side of the pipe or the connection
/// fails. A consumer that exits mid-turn ends this process; Milestone C of
/// `.agents/plans/mj-acp-adapter.md` covers what has to happen to the turns it
/// leaves behind.
pub(crate) async fn serve() -> Result<()> {
    Agent
        .builder()
        .name("mjolnir")
        .on_receive_request(
            async |request: InitializeRequest, responder, _cx| {
                responder.respond(initialize(request))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(Stdio::new())
        .await
        .context("serving the Agent Client Protocol on standard input and output")
}

/// Answer `initialize`.
///
/// This adapter speaks ACP v1 and claims no optional capability. The workspace,
/// the shell, and the files belong to the session's own worker on its target,
/// so there is nothing for the consumer to provide and nothing to advertise:
/// a capability promised here would invite a consumer to hand over work this
/// process has no business accepting.
fn initialize(_request: InitializeRequest) -> InitializeResponse {
    InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(AgentCapabilities::new())
}
